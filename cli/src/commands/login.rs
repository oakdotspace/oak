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

use std::io::{BufRead, Write};
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

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = write!(out, "Paste the code shown in the browser: ");
    let _ = out.flush();

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

// The success page is also a little stay-a-while game: click on the grass to
// plant oak trees. Self-contained — no external assets, since this is served
// once from a one-shot loopback listener that exits the moment we hand the
// token back to the CLI.
const OK_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Oak CLI authorized</title>
<style>
html,body{margin:0;padding:0;height:100%}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#1f2937;background:linear-gradient(to bottom,#e0f2fe 0%,#f0f9ff 55%,#fde68a 60%,#a3e635 60.2%,#65a30d 100%);overflow:hidden;user-select:none;cursor:crosshair}
.banner{position:absolute;top:36px;left:50%;transform:translateX(-50%);background:rgba(255,255,255,0.95);border:1px solid #d1d5db;border-radius:12px;padding:18px 32px;text-align:center;box-shadow:0 6px 24px rgba(0,0,0,0.08);z-index:10;max-width:440px}
.banner h1{margin:0 0 4px;font-size:22px;color:#15803d}
.banner p{margin:0;color:#4b5563;font-size:14px}
.counter{position:absolute;top:36px;right:32px;background:rgba(255,255,255,0.92);border-radius:999px;padding:8px 14px;font-size:13px;color:#15803d;font-weight:600;z-index:10;box-shadow:0 2px 10px rgba(0,0,0,0.06)}
.hint{position:absolute;bottom:24px;left:50%;transform:translateX(-50%);color:#166534;font-size:13px;background:rgba(255,255,255,0.78);padding:6px 16px;border-radius:999px;z-index:10;pointer-events:none}
#stage{position:absolute;inset:0;width:100%;height:100%}
.tree{transform:scale(0);transform-origin:0 0;transition:transform 1.6s cubic-bezier(.2,.8,.2,1.05)}
.acorn{transform:translateY(-40px);opacity:0;transition:transform .55s cubic-bezier(.4,1.4,.5,1),opacity .55s ease}
.acorn.dropped{transform:translateY(0);opacity:1}
</style></head>
<body>
<svg id="stage" xmlns="http://www.w3.org/2000/svg" aria-hidden="true"></svg>
<div class="counter">🌳 <span id="count">0</span> oak<span id="plural"></span></div>
<div class="banner"><h1>You're signed in</h1><p>You can close this tab and return to the terminal.</p></div>
<div class="hint">…or click the grass to plant some oak trees 🌰</div>
<script>
(function(){
const NS='http://www.w3.org/2000/svg';
const stage=document.getElementById('stage');
const countEl=document.getElementById('count');
const pluralEl=document.getElementById('plural');
const GREENS=['#166534','#15803d','#14532d','#3f6212','#4d7c0f'];
const BARKS=['#78350f','#7c2d12','#92400e'];
let count=0;
function groundY(){return window.innerHeight*0.6}
function sizeStage(){stage.setAttribute('viewBox','0 0 '+window.innerWidth+' '+window.innerHeight)}
sizeStage();window.addEventListener('resize',sizeStage);
function el(name,attrs){const n=document.createElementNS(NS,name);for(const k in attrs)n.setAttribute(k,attrs[k]);return n}
function plant(x,y){
  if(y<groundY())return false;
  const r=Math.random;
  const trunk=28+r()*30, crown=36+r()*26, lean=(r()-0.5)*6;
  const tint=GREENS[(r()*GREENS.length)|0], bark=BARKS[(r()*BARKS.length)|0];
  const wrap=el('g',{transform:'translate('+x+' '+y+')'});
  // Falling acorn that lands and turns into the tree.
  const acorn=el('g',{class:'acorn'});
  acorn.appendChild(el('ellipse',{cx:0,cy:-4,rx:4,ry:5,fill:'#92400e'}));
  acorn.appendChild(el('path',{d:'M -4 -7 Q 0 -11 4 -7 Z',fill:'#451a03'}));
  wrap.appendChild(acorn);
  // The tree itself, starts hidden.
  const tree=el('g',{class:'tree'});
  tree.appendChild(el('rect',{x:-4,y:-trunk,width:8,height:trunk,rx:2,fill:bark}));
  const lobes=5;
  for(let i=0;i<lobes;i++){
    const a=(i/lobes)*Math.PI*2;
    tree.appendChild(el('circle',{cx:Math.cos(a)*crown*0.45,cy:-trunk-6+Math.sin(a)*crown*0.35,r:crown*0.5,fill:tint,'fill-opacity':0.94}));
  }
  // Center crown blob for a fuller canopy.
  tree.appendChild(el('circle',{cx:0,cy:-trunk-8,r:crown*0.42,fill:tint}));
  // A couple of decorative acorns on the tree.
  const nAcorns=2+((r()*3)|0);
  for(let i=0;i<nAcorns;i++){
    tree.appendChild(el('circle',{cx:(r()-0.5)*crown*0.8,cy:-trunk-4+(r()-0.5)*crown*0.55,r:2.4,fill:'#7c2d12'}));
  }
  wrap.appendChild(tree);
  stage.appendChild(wrap);
  // Sort wraps by their translate-y so trees lower on screen render in front.
  const kids=Array.prototype.slice.call(stage.children);
  kids.sort((a,b)=>{const ma=/translate\([^ ]+ ([-\d.]+)\)/.exec(a.getAttribute('transform'));const mb=/translate\([^ ]+ ([-\d.]+)\)/.exec(b.getAttribute('transform'));return parseFloat(ma[1])-parseFloat(mb[1])});
  kids.forEach(k=>stage.appendChild(k));
  // Animate: acorn drops, then tree springs up.
  requestAnimationFrame(()=>{acorn.classList.add('dropped')});
  setTimeout(()=>{acorn.style.opacity='0';tree.style.transform='scale(1) rotate('+lean+'deg)'},420);
  count++;countEl.textContent=count;pluralEl.textContent=count===1?'':'s';
  return true;
}
document.addEventListener('click',e=>plant(e.clientX,e.clientY));
// Seed a couple of trees so the world isn't empty when you arrive.
setTimeout(()=>plant(window.innerWidth*0.28,groundY()+30),300);
setTimeout(()=>plant(window.innerWidth*0.74,groundY()+60),650);
})();
</script></body></html>"#;

const FAIL_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Oak CLI authorization failed</title>
<style>body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#fafafa;color:#1f2937;display:flex;align-items:center;justify-content:center;min-height:100vh;margin:0}.card{background:#fff;border:1px solid #e5e7eb;border-radius:12px;padding:32px 40px;text-align:center;max-width:420px}h1{margin:0 0 8px;font-size:22px;color:#b91c1c}p{margin:0;color:#4b5563;font-size:14px}</style>
</head><body><div class="card"><h1>Authorization failed</h1><p>Return to the terminal and run <code>oak login</code> again.</p></div></body></html>"#;
