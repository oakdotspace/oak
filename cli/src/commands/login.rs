//! `oak login` — browser-based device authorization.
//!
//! Two flows share a single web endpoint at `/cli-auth`:
//!
//! - **Loopback** (default): bind a one-shot HTTP listener on a free
//!   loopback port, open the user's browser at
//!   `{remote}/cli-auth?callback=…&state=…`, wait for the 302 back with
//!   `?token=…&username=…&state=…`, validate the state, save the credential.
//! - **Manual paste fallback**: if `open::that` errors (typical on a
//!   headless VM / SSH session / container with no browser launcher), drop
//!   the loopback listener and print the bare `{remote}/cli-auth` URL.
//!   The user opens it on any other device, authorizes, and the web shows
//!   a one-time `username:token` code to paste back into this prompt.
//!
//! There's no username/password prompt anymore — the browser owns the
//! authentication step (password, GitHub OAuth, MFA, whatever the web
//! supports). The CLI just trades a one-time loopback callback (or a
//! manual paste) for an API token.

use std::io::BufRead;
use std::time::Duration;

use oak_core::{OakError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::output;

use super::credentials::{save_credential, Credential};

/// Hard upper bound on how long we'll wait for the browser callback before
/// giving up. Long enough for a user to read the page, click through, and
/// even sign in from scratch — short enough that a stranded CLI eventually
/// dies on its own.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

pub fn run(remote: &str) -> Result<()> {
    let remote = remote.trim_end_matches('/').to_string();

    let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    rt.block_on(login_and_save(&remote))?;
    Ok(())
}

/// Run the browser login flow for `remote`, persist the credential, and
/// report success. Returns the logged-in username. Callers already inside a
/// tokio runtime (e.g. the interactive push flow) use this directly instead
/// of [`run`], which spins up its own runtime and would panic if nested.
pub async fn login_and_save(remote: &str) -> Result<String> {
    let remote = remote.trim_end_matches('/').to_string();
    let (token, username) = run_browser_login(&remote).await?;

    save_credential(Credential {
        server: remote.clone(),
        token,
        username: username.clone(),
    })?;

    output::success(&format!("Logged in as '{username}' on {remote}"));
    Ok(username)
}

async fn run_browser_login(remote: &str) -> Result<(String, String)> {
    // Bind first so we have a real port to put in the URL we ship to the
    // browser. 127.0.0.1 (not 0.0.0.0) — we don't want anything off this
    // machine reaching the listener while it's open.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| OakError::Io(std::io::Error::other(format!("bind loopback: {e}"))))?;
    let port = listener
        .local_addr()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?
        .port();

    let csrf = uuid::Uuid::new_v4().simple().to_string();
    let callback = format!("http://127.0.0.1:{port}");
    let url = format!(
        "{remote}/cli-auth?callback={cb}&state={st}",
        cb = urlencoding::encode(&callback),
        st = urlencoding::encode(&csrf),
    );

    output::info(&format!("Opening browser to authorize this device: {url}"));
    if let Err(e) = open::that(&url) {
        // Headless box: drop the loopback listener and fall through to a
        // manual paste flow. We tear down the listener explicitly so the
        // port doesn't linger while the user is off authorizing on
        // another device.
        drop(listener);
        output::warning(&format!(
            "Could not open a browser on this machine ({e}). Switching to manual mode."
        ));
        return run_manual_login(remote);
    }
    output::info("Waiting for browser authorization (Ctrl-C to cancel)…");

    let accept = listener.accept();
    let (stream, _addr) = tokio::time::timeout(CALLBACK_TIMEOUT, accept)
        .await
        .map_err(|_| {
            OakError::Server(
                "Timed out waiting for browser authorization. Re-run `oak login` to try again."
                    .to_string(),
            )
        })?
        .map_err(|e| OakError::Io(std::io::Error::other(format!("accept: {e}"))))?;

    handle_callback(stream, &csrf).await
}

/// Headless fallback: print the bare /cli-auth URL, ask the user to open
/// it on any browser, and read the `username:token` code they paste back.
fn run_manual_login(remote: &str) -> Result<(String, String)> {
    let url = format!("{remote}/cli-auth");
    output::info(&format!(
        "Open this URL on any device, sign in, and click Authorize: {url}"
    ));

    output::print_raw("Paste the code shown in the browser: ");

    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|e| OakError::Io(std::io::Error::other(format!("read code: {e}"))))?;
    let raw = line.trim();
    if raw.is_empty() {
        return Err(OakError::Server(
            "No code was pasted. Re-run `oak login` to try again.".to_string(),
        ));
    }

    let (username, token) = raw.split_once(':').ok_or_else(|| {
        OakError::Server(
            "Code wasn't in `username:token` form. Copy the whole code shown in the browser."
                .to_string(),
        )
    })?;
    let username = username.trim();
    let token = token.trim();
    if username.is_empty() || token.is_empty() {
        return Err(OakError::Server(
            "Code was malformed (empty username or token). Re-run `oak login`.".to_string(),
        ));
    }
    Ok((token.to_string(), username.to_string()))
}

/// Read the single inbound GET request, parse `token`/`username`/`state` out
/// of its query string, write back a friendly "you can close this tab" page,
/// and return the credentials.
async fn handle_callback(
    mut stream: tokio::net::TcpStream,
    expected_state: &str,
) -> Result<(String, String)> {
    // Read just enough bytes to capture the request line + headers. Modern
    // browsers send a GET with no body for a 302 follow-up, so we don't need
    // a full HTTP parser — bound the read so a malicious client can't pin us.
    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| OakError::Io(std::io::Error::other(format!("read callback: {e}"))))?;
    let raw = String::from_utf8_lossy(&buf[..n]);

    // Request line is the first whitespace-separated token after the verb:
    //   GET /?token=…&username=…&state=… HTTP/1.1
    let request_line = raw.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let _method = parts.next();
    let path = parts.next().unwrap_or("");

    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

    let (mut token, mut username, mut state, mut error) = (
        None::<String>,
        None::<String>,
        None::<String>,
        None::<String>,
    );
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let decoded = urlencoding::decode(v).map(|c| c.into_owned()).ok();
        match k {
            "token" => token = decoded,
            "username" => username = decoded,
            "state" => state = decoded,
            "error" => error = decoded,
            _ => {}
        }
    }

    if let Some(err) = error.as_deref() {
        let _ = respond(&mut stream, FAIL_PAGE).await;
        return Err(OakError::Server(format!(
            "Authorization failed in browser: {err}"
        )));
    }

    let state = state.ok_or_else(|| {
        OakError::Server("Callback was missing the state token. Re-run `oak login`.".to_string())
    })?;
    if state != expected_state {
        let _ = respond(&mut stream, FAIL_PAGE).await;
        return Err(OakError::Server(
            "Callback state did not match. Re-run `oak login`.".to_string(),
        ));
    }

    let token = token.ok_or_else(|| {
        OakError::Server("Callback was missing the token. Re-run `oak login`.".to_string())
    })?;
    let username = username.ok_or_else(|| {
        OakError::Server("Callback was missing the username. Re-run `oak login`.".to_string())
    })?;

    let _ = respond(&mut stream, OK_PAGE).await;
    Ok((token, username))
}

async fn respond(stream: &mut tokio::net::TcpStream, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

// The success page doubles as a little stay-a-while game: a monochrome
// "garden" memory match — flip the tiles to pair up Yarndings 20 glyphs
// (flowers, a leaf, a heart, a cat, a smiley). Styled to oak.space: black
// & white, monospace, square tiles, with a matched pair inverting to a
// solid fill the way the homepage's buttons do. The only external asset is
// the Yarndings 20 webfont, which the *browser* pulls from Google Fonts
// (not this loopback listener); if it can't, the tiles fall back to plain
// letters and the game still plays. Otherwise self-contained, since this is
// served once from a one-shot loopback listener that exits the moment we
// hand the token back to the CLI.
const OK_PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>oak · signed in</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Yarndings+20&display=swap" rel="stylesheet">
<style>
:root{
  --bg:255 255 255;--fg:0 0 0;--border:218 218 218;
  --tile:248 248 248;--tile-line:198 198 198;
  --mono:'Geist Mono','JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,monospace;
  --yarn:'Yarndings 20',var(--mono);
}
@media(prefers-color-scheme:dark){:root{
  --bg:0 0 0;--fg:255 255 255;--border:64 64 64;
  --tile:22 22 22;--tile-line:84 84 84;
}}
*{box-sizing:border-box}
html,body{height:100%}
body{margin:0;background:rgb(var(--bg));color:rgb(var(--fg));font-family:var(--mono);
  font-size:14px;line-height:1.55;-webkit-font-smoothing:antialiased;
  display:flex;align-items:center;justify-content:center;padding:24px;user-select:none}
.wrap{width:100%;max-width:432px}
.brand{display:flex;align-items:center;gap:7px;margin-bottom:22px;font-weight:600;letter-spacing:-.02em}
.brand .mark{width:16px;height:16px}
.brand .name{font-size:14px}
h1{margin:0 0 5px;font-size:16px;font-weight:600;letter-spacing:-.01em}
h1 .ok{margin-right:8px;font-weight:400}
.sub{margin:0 0 22px;color:rgb(var(--fg)/.62)}
.board{display:grid;grid-template-columns:repeat(4,1fr);gap:8px}
.card{aspect-ratio:1;background:none;border:0;padding:0;perspective:680px;cursor:pointer;font-family:inherit}
.card[disabled]{cursor:default}
.inner{display:block;position:relative;width:100%;height:100%;transform-style:preserve-3d;
  transition:transform .42s cubic-bezier(.2,.7,.2,1)}
.card.flipped .inner{transform:rotateY(180deg)}
.face{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;
  border:1px solid rgb(var(--tile-line));backface-visibility:hidden;-webkit-backface-visibility:hidden}
.back{background:rgb(var(--tile))}
.back .m{width:18px;height:18px;color:rgb(var(--fg)/.32)}
.front{background:rgb(var(--bg));transform:rotateY(180deg);
  font-family:var(--yarn);font-size:clamp(24px,7.4vw,36px);color:rgb(var(--fg))}
.card:not([disabled]):hover .back{border-color:rgb(var(--fg)/.55);background:rgb(var(--fg)/.08)}
.card.matched .front{background:rgb(var(--fg));border-color:rgb(var(--fg));color:rgb(var(--bg))}
.card.matched .inner{animation:pop .45s ease}
@keyframes pop{0%{transform:rotateY(180deg) scale(1)}45%{transform:rotateY(180deg) scale(1.09)}
  100%{transform:rotateY(180deg) scale(1)}}
.status{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-top:20px;color:rgb(var(--fg)/.62)}
.counts{font-variant-numeric:tabular-nums}
.counts b{color:rgb(var(--fg));font-weight:600}
.again{font-family:inherit;font-size:13px;font-weight:600;color:rgb(var(--bg));background:rgb(var(--fg));
  border:1px solid rgb(var(--fg));padding:6px 14px;cursor:pointer;transition:opacity .15s ease}
.again:hover{opacity:.85}
.msg{min-height:1.3em;margin:14px 0 0;color:rgb(var(--fg)/.62)}
.msg.win{color:rgb(var(--fg));font-weight:600}
.msg .y{font-family:var(--yarn);font-weight:400}
</style></head>
<body>
<div class="wrap">
  <svg style="position:absolute;width:0;height:0" aria-hidden="true"><symbol id="oak-mark" viewBox="0 0 19 19" fill="currentColor" shape-rendering="crispEdges"><rect x="9" y="0" width="1" height="1"/><rect x="8" y="1" width="3" height="1"/><rect x="7" y="2" width="5" height="1"/><rect x="6" y="3" width="7" height="1"/><rect x="9" y="4" width="1" height="1"/><rect x="9" y="5" width="1" height="1"/><rect x="3" y="6" width="1" height="1"/><rect x="6" y="6" width="7" height="1"/><rect x="15" y="6" width="1" height="1"/><rect x="2" y="7" width="2" height="1"/><rect x="6" y="7" width="7" height="1"/><rect x="15" y="7" width="2" height="1"/><rect x="1" y="8" width="3" height="1"/><rect x="6" y="8" width="3" height="1"/><rect x="10" y="8" width="3" height="1"/><rect x="15" y="8" width="3" height="1"/><rect x="0" y="9" width="8" height="1"/><rect x="11" y="9" width="8" height="1"/><rect x="1" y="10" width="3" height="1"/><rect x="6" y="10" width="3" height="1"/><rect x="10" y="10" width="3" height="1"/><rect x="15" y="10" width="3" height="1"/><rect x="2" y="11" width="2" height="1"/><rect x="6" y="11" width="7" height="1"/><rect x="15" y="11" width="2" height="1"/><rect x="3" y="12" width="1" height="1"/><rect x="6" y="12" width="7" height="1"/><rect x="15" y="12" width="1" height="1"/><rect x="9" y="13" width="1" height="1"/><rect x="9" y="14" width="1" height="1"/><rect x="6" y="15" width="7" height="1"/><rect x="7" y="16" width="5" height="1"/><rect x="8" y="17" width="3" height="1"/><rect x="9" y="18" width="1" height="1"/></symbol></svg>
  <div class="brand"><svg class="mark" aria-hidden="true"><use href="#oak-mark"/></svg><span class="name">oak</span></div>
  <h1><span class="ok">✓</span>you're signed in</h1>
  <p class="sub">close this tab and head back to your terminal — or tend the garden while you wait.</p>
  <div class="board" id="board" aria-label="memory match garden"></div>
  <div class="status">
    <span class="counts">moves <b id="moves">0</b> &middot; pairs <b id="pairs">0</b>/8</span>
    <button class="again" id="again" type="button">plant again ↻</button>
  </div>
  <p class="msg" id="msg"></p>
</div>
<script>
(function(){
  // Each letter renders as a Yarndings 20 glyph: f flower · d daisy · h leaf
  // · E clover · y heart · u smile · { cat · W sparkle. Match the pairs.
  var SYMBOLS=["f","d","h","E","y","u","{","W"];
  var board=document.getElementById("board"),
      movesEl=document.getElementById("moves"),
      pairsEl=document.getElementById("pairs"),
      msgEl=document.getElementById("msg"),
      again=document.getElementById("again");
  var first=null,lock=false,moves=0,pairs=0;
  function shuffle(a){for(var i=a.length-1;i>0;i--){var j=Math.floor(Math.random()*(i+1)),t=a[i];a[i]=a[j];a[j]=t;}return a;}
  function reset(){
    board.innerHTML="";first=null;lock=false;moves=0;pairs=0;
    movesEl.textContent="0";pairsEl.textContent="0";msgEl.textContent="";msgEl.className="msg";
    shuffle(SYMBOLS.concat(SYMBOLS)).forEach(function(sym){
      var c=document.createElement("button");
      c.type="button";c.className="card";c.dataset.sym=sym;c.setAttribute("aria-label","card");
      c.innerHTML='<span class="inner"><span class="face back"><svg class="m" aria-hidden="true"><use href="#oak-mark"/></svg></span>'+
                  '<span class="face front">'+sym+'</span></span>';
      c.addEventListener("click",function(){flip(c);});
      board.appendChild(c);
    });
  }
  function flip(c){
    if(lock||c.classList.contains("flipped")||c.classList.contains("matched"))return;
    c.classList.add("flipped");
    if(!first){first=c;return;}
    moves++;movesEl.textContent=moves;
    if(c.dataset.sym===first.dataset.sym){
      c.classList.add("matched");first.classList.add("matched");
      c.disabled=true;first.disabled=true;first=null;
      pairs++;pairsEl.textContent=pairs;
      if(pairs===SYMBOLS.length){
        msgEl.className="msg win";
        msgEl.innerHTML='garden complete in '+moves+' moves &nbsp;<span class="y">d</span>';
      }
    }else{
      lock=true;var a=first,b=c;first=null;
      setTimeout(function(){a.classList.remove("flipped");b.classList.remove("flipped");lock=false;},720);
    }
  }
  again.addEventListener("click",reset);
  reset();
})();
</script>
</body></html>"##;

const FAIL_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>oak · authorization failed</title>
<style>
:root{--bg:255 255 255;--fg:0 0 0}
@media(prefers-color-scheme:dark){:root{--bg:0 0 0;--fg:255 255 255}}
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;
  background:rgb(var(--bg));color:rgb(var(--fg));padding:24px;
  font-family:'Geist Mono','JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,monospace;
  font-size:14px;line-height:1.55;-webkit-font-smoothing:antialiased}
.card{max-width:420px}
.mark{display:flex;align-items:center;gap:7px;font-weight:600;letter-spacing:-.02em;margin-bottom:18px}
.mark svg{width:16px;height:16px}
h1{margin:0 0 6px;font-size:16px;font-weight:600;letter-spacing:-.01em}
p{margin:0;color:rgb(var(--fg)/.62)}
code{font-family:inherit;border:1px solid rgb(var(--fg)/.25);padding:1px 6px}
</style></head>
<body><div class="card"><div class="mark"><svg viewBox="0 0 19 19" fill="currentColor" shape-rendering="crispEdges" aria-hidden="true"><rect x="9" y="0" width="1" height="1"/><rect x="8" y="1" width="3" height="1"/><rect x="7" y="2" width="5" height="1"/><rect x="6" y="3" width="7" height="1"/><rect x="9" y="4" width="1" height="1"/><rect x="9" y="5" width="1" height="1"/><rect x="3" y="6" width="1" height="1"/><rect x="6" y="6" width="7" height="1"/><rect x="15" y="6" width="1" height="1"/><rect x="2" y="7" width="2" height="1"/><rect x="6" y="7" width="7" height="1"/><rect x="15" y="7" width="2" height="1"/><rect x="1" y="8" width="3" height="1"/><rect x="6" y="8" width="3" height="1"/><rect x="10" y="8" width="3" height="1"/><rect x="15" y="8" width="3" height="1"/><rect x="0" y="9" width="8" height="1"/><rect x="11" y="9" width="8" height="1"/><rect x="1" y="10" width="3" height="1"/><rect x="6" y="10" width="3" height="1"/><rect x="10" y="10" width="3" height="1"/><rect x="15" y="10" width="3" height="1"/><rect x="2" y="11" width="2" height="1"/><rect x="6" y="11" width="7" height="1"/><rect x="15" y="11" width="2" height="1"/><rect x="3" y="12" width="1" height="1"/><rect x="6" y="12" width="7" height="1"/><rect x="15" y="12" width="1" height="1"/><rect x="9" y="13" width="1" height="1"/><rect x="9" y="14" width="1" height="1"/><rect x="6" y="15" width="7" height="1"/><rect x="7" y="16" width="5" height="1"/><rect x="8" y="17" width="3" height="1"/><rect x="9" y="18" width="1" height="1"/></svg><span>oak</span></div>
<h1>authorization failed</h1>
<p>head back to your terminal and run <code>oak login</code> again.</p>
</div></body></html>"#;
