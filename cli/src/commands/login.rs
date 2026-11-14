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
//! `OAK_BROWSER`, when set, names the command that opens the URL (run as
//! `$OAK_BROWSER <url>`, split on whitespace) instead of the system opener;
//! a command that fails counts as "no browser" and takes the manual path.
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
    if let Err(e) = open_browser(&url) {
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

/// Open `url` with `$OAK_BROWSER` when set, else the system default browser.
/// Like the system opener, the command should return once it has handed the
/// URL off; a non-zero exit is an error.
fn open_browser(url: &str) -> std::io::Result<()> {
    let browser = std::env::var("OAK_BROWSER").unwrap_or_default();
    let mut words = browser.split_whitespace();
    let Some(program) = words.next() else {
        return open::that(url);
    };
    let status = std::process::Command::new(program)
        .args(words)
        .arg(url)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "OAK_BROWSER `{browser}` exited with {status}"
        )))
    }
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
        let _ = respond(&mut stream, &fail_page()).await;
        return Err(OakError::Server(format!(
            "Authorization failed in browser: {err}"
        )));
    }

    let state = state.ok_or_else(|| {
        OakError::Server("Callback was missing the state token. Re-run `oak login`.".to_string())
    })?;
    if state != expected_state {
        let _ = respond(&mut stream, &fail_page()).await;
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

    let _ = respond(&mut stream, &ok_page(&username)).await;
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

// The pages the browser lands on once `/cli-auth` sends it back here. They
// look like oak.space's own sign-in pages (its gallery theme): the Oak mark
// at the top left, then a narrow centred column of small monospace text,
// black on white (reversed in dark mode), with nothing boxed but the things
// you press. The success page keeps a little memory match to play while
// you're here: flip the tiles to pair up Yarndings 20 glyphs, a matched
// pair inverting to a solid fill the way oak.space's buttons do. The browser
// pulls Geist Mono and Yarndings 20 from Google Fonts (not this loopback
// listener); without them the text falls back to the system monospace face
// and the tiles show plain letters. Otherwise self-contained, since this is
// served once from a one-shot listener that exits the moment we hand the
// token back to the CLI.

/// The Oak mark (oak.space's `layout::OAK_MARK_SVG`, also its favicon), as
/// one path in a 512 × 512 box.
pub(crate) const OAK_MARK_PATH: &str = "M173 79Q166 80 160 82Q153 84 147 86Q141 89 135 92Q129 95 123 99Q117 102 112 107Q107 111 102 115Q97 120 93 125Q88 130 84 136Q80 141 77 147Q74 153 71 159Q68 165 66 171Q64 178 62 184Q61 191 60 198Q59 204 58 211Q58 218 58 225Q59 231 60 238Q61 245 63 252Q64 258 67 265Q69 271 72 277Q75 284 78 289Q82 295 86 301Q90 307 95 312Q99 317 104 321Q110 326 115 330Q121 334 127 338Q133 342 139 345Q146 348 152 350Q159 352 166 354Q172 356 179 357Q186 358 193 358Q200 359 207 359Q214 359 220 359Q226 358 232 358Q238 358 242 358Q247 359 251 362Q255 365 258 369Q262 374 266 379Q269 385 273 390Q277 396 282 401Q286 406 292 411Q297 416 303 420Q308 424 315 428Q321 431 328 434Q335 436 342 438Q349 439 356 440Q364 440 371 440Q378 439 386 437Q393 435 400 433Q407 430 413 426Q419 422 425 418Q431 413 436 407Q441 402 445 396Q449 389 452 383Q456 376 458 369Q460 362 461 354Q462 347 462 340Q462 332 461 325Q459 318 457 311Q455 304 452 297Q449 291 445 285Q441 279 436 274Q431 268 426 264Q420 259 414 255Q408 251 402 248Q396 245 390 243Q383 240 377 238Q371 236 365 234Q359 232 355 229Q350 226 348 222Q345 218 344 213Q343 208 341 201Q340 195 338 189Q337 182 334 176Q332 169 330 163Q327 157 323 150Q320 144 316 139Q312 133 308 127Q303 122 298 117Q294 112 288 108Q283 104 277 100Q271 96 265 93Q259 89 253 87Q246 84 240 82Q233 80 227 79Q220 78 213 77Q206 76 200 76Q193 76 186 77Q179 78 173 79ZM199 88Q205 88 212 89Q218 90 224 92Q230 93 235 96Q241 98 247 100Q252 103 257 106Q263 109 267 113Q272 117 277 121Q281 125 285 130Q289 134 293 139Q296 144 299 149Q302 154 305 160Q308 165 310 171Q312 177 314 183Q315 188 317 195Q319 201 320 207Q322 214 325 221Q328 228 334 234Q339 240 346 243Q353 246 360 248Q366 250 372 252Q378 254 384 256Q390 259 395 262Q400 265 405 269Q409 272 414 277Q418 281 421 286Q425 290 428 295Q430 300 432 306Q434 311 436 317Q437 322 437 328Q438 334 437 339Q437 345 436 350Q435 356 433 361Q431 366 429 371Q426 376 423 381Q420 385 416 389Q413 393 408 397Q404 401 399 403Q395 406 390 409Q384 411 379 412Q374 414 368 415Q363 416 357 416Q351 416 346 415Q340 414 335 413Q329 411 324 409Q318 407 313 404Q308 401 304 397Q299 393 295 389Q291 385 287 380Q283 375 280 370Q276 364 272 358Q268 352 262 347Q257 341 249 338Q241 335 233 335Q226 334 220 334Q213 335 207 335Q201 335 194 335Q188 334 182 334Q176 333 171 332Q165 331 159 329Q153 327 148 325Q142 323 137 320Q131 318 126 314Q121 311 117 307Q112 304 107 299Q103 295 99 290Q95 286 92 281Q88 276 85 270Q82 265 80 259Q78 254 76 248Q74 242 73 236Q71 230 71 224Q70 218 70 211Q70 205 71 199Q71 193 72 187Q74 180 75 175Q77 169 80 163Q82 157 85 152Q88 146 92 141Q95 136 100 131Q104 126 108 122Q113 118 118 114Q123 110 128 107Q133 103 139 100Q144 98 150 96Q156 93 162 92Q168 90 175 89Q181 88 187 88Q193 88 199 88ZM176 113Q169 115 163 117Q156 119 150 122Q144 125 138 129Q133 133 128 138Q123 142 118 147Q114 153 110 158Q106 164 103 170Q100 176 98 183Q96 189 95 196Q93 203 93 209Q92 216 93 223Q93 230 94 237Q95 244 98 250Q100 257 103 263Q106 270 110 276Q114 281 118 287Q123 292 128 297Q133 302 139 306Q145 310 152 313Q158 316 165 318Q172 321 179 322Q186 323 193 324Q201 324 208 324Q215 323 222 322Q229 321 236 319Q243 316 249 314Q256 311 262 307Q269 304 275 299Q281 294 286 289Q291 283 295 276Q298 269 301 262Q303 255 304 248Q306 241 306 233Q307 226 307 219Q307 212 306 205Q305 198 303 191Q301 184 298 177Q296 171 292 164Q289 158 284 152Q280 147 275 142Q270 137 264 132Q259 128 252 125Q246 121 240 118Q233 116 227 114Q220 112 213 111Q206 110 199 110Q193 110 187 111Q182 112 176 113ZM199 122Q205 123 211 124Q217 125 223 127Q228 129 234 132Q239 135 244 138Q249 142 253 146Q258 150 261 154Q265 159 268 164Q271 169 274 174Q276 179 278 185Q280 190 281 196Q282 202 282 207Q283 213 283 219Q282 225 282 231Q281 236 279 242Q278 248 276 253Q274 258 272 263Q269 267 266 271Q263 275 258 278Q254 281 249 284Q244 287 239 290Q233 292 228 294Q222 296 217 297Q211 299 206 299Q200 300 194 300Q188 300 183 299Q177 299 171 297Q166 296 160 294Q155 291 150 289Q145 286 140 282Q135 279 131 275Q127 271 123 266Q119 261 116 256Q113 251 111 245Q109 240 107 234Q105 228 105 222Q104 216 104 210Q104 204 105 198Q106 192 108 186Q109 180 112 174Q115 169 118 163Q121 158 125 153Q129 149 134 145Q138 140 144 137Q149 134 154 131Q160 128 166 126Q172 124 178 123Q184 122 188 122Q193 122 199 122ZM339 266Q333 267 326 270Q320 272 314 275Q308 279 303 284Q297 289 294 295Q290 301 288 307Q286 314 285 321Q284 328 284 335Q284 342 285 349Q287 356 289 362Q291 369 295 376Q298 382 303 388Q307 393 313 398Q319 403 325 407Q332 410 339 413Q346 415 354 416Q361 417 369 416Q376 416 384 413Q391 411 398 408Q405 404 411 399Q417 394 422 388Q427 382 430 375Q434 368 436 361Q438 353 438 345Q439 338 438 330Q437 323 434 316Q432 308 428 302Q424 296 419 290Q414 285 408 280Q402 276 396 272Q389 269 383 267Q376 265 369 264Q362 264 355 264Q348 264 347 264Q346 265 339 266ZM355 276Q361 276 367 277Q372 278 378 281Q383 283 388 286Q393 289 397 293Q401 297 404 302Q407 306 410 311Q412 317 413 322Q414 327 414 332Q415 338 414 343Q413 348 411 353Q410 358 407 363Q405 367 401 371Q398 375 394 379Q390 382 386 385Q381 387 376 389Q371 391 366 392Q361 393 355 392Q350 392 345 391Q339 390 334 388Q329 386 325 383Q320 380 316 376Q312 372 308 367Q305 362 302 357Q300 352 298 346Q297 340 296 334Q296 328 296 322Q296 316 298 310Q299 305 302 300Q305 294 309 291Q313 287 319 284Q324 281 330 279Q336 277 342 276Q348 275 348 275Q348 276 355 276ZM177 148Q171 150 165 153Q158 157 153 161Q148 165 143 171Q139 176 135 182Q132 188 130 195Q128 202 127 209Q126 216 127 223Q127 230 129 237Q131 244 134 250Q138 257 142 263Q147 268 153 273Q159 278 165 281Q172 285 179 287Q187 289 194 290Q202 290 210 289Q217 288 225 285Q232 282 239 278Q245 274 251 268Q256 263 260 256Q265 250 267 242Q270 235 271 227Q272 220 272 212Q271 204 269 197Q267 189 263 183Q260 176 255 170Q250 165 244 160Q238 156 232 152Q225 149 218 147Q212 145 204 145Q197 144 191 145Q184 146 177 148ZM203 157Q209 158 214 160Q219 163 224 166Q229 169 233 173Q236 177 239 182Q242 186 244 192Q246 197 247 202Q248 207 248 213Q248 218 247 223Q245 228 243 233Q241 238 238 242Q236 246 232 250Q228 253 224 256Q220 259 215 261Q210 263 205 264Q200 266 195 266Q190 266 184 265Q179 264 174 262Q169 260 164 258Q159 255 155 251Q151 247 148 242Q145 238 142 232Q140 227 139 221Q138 215 138 210Q138 204 140 198Q141 192 144 187Q147 181 150 177Q154 172 159 168Q164 164 169 162Q174 159 180 158Q186 156 192 156Q197 156 203 157ZM343 292Q336 295 331 299Q325 304 321 309Q316 315 314 322Q311 329 311 336Q310 343 311 351Q313 358 316 365Q320 372 326 377Q331 383 338 387Q345 391 353 392Q361 394 370 392Q378 391 385 388Q393 384 399 378Q405 372 409 365Q413 357 414 349Q416 341 414 333Q413 325 409 318Q406 310 400 305Q395 299 388 295Q381 291 374 290Q367 288 360 289Q353 289 351 289Q349 290 343 292ZM359 301Q365 301 370 303Q375 305 379 309Q383 312 386 316Q389 321 390 325Q391 330 391 335Q391 340 390 344Q389 349 386 352Q384 356 381 360Q378 363 374 365Q370 367 365 368Q361 369 356 369Q351 369 346 368Q342 366 337 363Q333 361 330 356Q327 352 325 347Q323 342 322 336Q322 331 324 325Q325 320 328 315Q332 310 336 307Q341 304 347 302Q353 300 353 300Q353 300 359 301ZM181 183Q174 187 169 192Q165 198 162 206Q160 213 161 221Q162 229 166 236Q170 243 177 248Q184 253 193 254Q202 256 210 253Q219 250 226 244Q232 237 235 228Q238 220 236 211Q235 202 230 195Q225 188 218 184Q211 179 203 179Q196 178 191 179Q187 180 181 183ZM200 192Q205 193 208 197Q211 200 213 204Q214 208 214 212Q214 216 212 220Q210 223 208 226Q205 228 202 230Q198 232 194 232Q190 232 186 231Q182 229 179 226Q175 223 174 218Q172 213 173 208Q174 203 177 199Q181 194 186 192Q191 190 193 190Q195 190 200 192ZM348 316Q341 321 337 328Q333 335 334 344Q335 353 342 359Q348 366 358 368Q368 369 377 364Q385 359 389 349Q392 339 388 330Q385 321 377 317Q370 312 362 312Q355 312 348 316ZM366 329Q369 332 369 335Q369 338 368 341Q366 343 364 345Q362 346 359 347Q356 347 353 346Q350 344 348 340Q346 337 348 332Q349 328 353 326Q357 324 361 325Q364 326 366 329ZM170 40Q163 41 156 42Q150 44 143 46Q137 48 130 50Q124 53 118 56Q111 59 105 63Q99 66 94 70Q88 74 83 79Q78 83 73 88Q68 93 63 98Q59 103 55 109Q50 114 47 120Q43 126 40 132Q37 138 34 145Q31 151 29 157Q27 164 25 171Q23 177 22 184Q21 191 20 198Q20 205 20 212Q20 219 20 225Q20 232 21 239Q22 246 24 253Q25 260 28 266Q30 273 32 279Q35 286 38 292Q41 298 44 304Q48 310 52 316Q56 321 60 326Q65 332 69 337Q74 342 79 346Q85 351 90 355Q96 359 101 363Q107 367 113 370Q119 373 126 376Q132 379 139 381Q145 383 152 385Q158 387 165 388Q172 389 178 390Q185 391 192 392Q198 393 205 393Q211 394 217 395Q223 396 228 397Q233 399 238 402Q243 404 247 408Q252 412 257 416Q262 420 267 425Q272 429 278 433Q283 437 289 441Q295 445 301 448Q308 451 314 453Q321 456 328 458Q334 459 341 460Q348 461 355 461Q363 462 370 461Q377 460 384 459Q391 458 397 456Q404 453 411 450Q417 448 423 444Q429 440 435 436Q441 432 446 427Q451 422 455 416Q460 411 464 405Q467 399 471 393Q474 386 476 379Q479 373 480 366Q482 359 483 352Q483 345 483 338Q483 331 483 324Q482 317 480 310Q479 303 476 296Q474 289 471 283Q468 277 464 271Q460 265 456 259Q452 254 447 249Q442 243 437 239Q431 234 426 230Q420 226 415 223Q409 219 404 215Q399 212 394 208Q389 205 386 201Q382 197 379 192Q376 187 374 182Q372 176 369 170Q367 164 365 158Q362 152 359 145Q356 139 353 133Q350 127 346 121Q343 116 339 110Q334 104 330 99Q325 94 321 89Q316 84 310 80Q305 75 300 71Q294 67 288 64Q282 60 276 57Q270 54 264 51Q257 48 251 46Q244 44 237 43Q231 41 224 40Q217 39 210 38Q203 38 196 38Q189 38 183 38Q177 39 170 40ZM196 59Q202 59 208 59Q214 60 220 61Q226 62 232 63Q238 64 244 66Q250 68 255 70Q261 73 267 76Q272 78 277 81Q282 85 287 88Q292 92 297 96Q301 100 306 104Q310 108 314 113Q318 118 322 122Q325 127 329 133Q332 138 335 143Q338 149 340 154Q343 160 345 166Q348 172 350 178Q352 184 355 190Q358 197 361 203Q365 209 370 215Q375 220 381 225Q387 229 392 233Q398 236 403 240Q408 243 413 247Q418 251 423 255Q427 259 432 263Q436 267 439 272Q443 277 446 282Q449 287 452 292Q455 297 456 303Q458 309 460 314Q461 320 462 326Q462 332 462 338Q462 344 462 350Q461 355 460 361Q458 367 456 372Q454 378 452 383Q449 389 446 393Q443 398 439 403Q435 408 431 412Q427 416 422 419Q417 423 412 426Q407 429 402 431Q397 434 391 436Q385 437 380 439Q374 440 368 440Q362 441 356 441Q350 440 344 439Q338 439 333 437Q327 436 321 434Q316 432 310 429Q305 426 300 423Q295 420 290 416Q285 413 280 409Q275 405 270 400Q266 396 260 392Q255 387 248 383Q242 380 235 377Q227 375 220 374Q213 373 207 372Q200 372 194 371Q188 370 181 370Q175 369 169 368Q163 366 157 365Q151 363 146 361Q140 359 134 357Q129 354 123 351Q118 348 113 345Q108 342 103 338Q98 335 93 331Q89 327 84 322Q80 318 76 313Q72 308 69 303Q65 298 62 293Q59 288 57 283Q54 277 52 271Q49 266 48 260Q46 254 44 248Q43 242 42 236Q41 230 41 224Q41 218 41 212Q41 206 41 200Q42 194 43 188Q44 182 45 176Q47 170 49 164Q51 158 53 153Q56 147 58 142Q61 136 64 131Q68 126 71 121Q75 116 79 112Q83 107 87 103Q92 99 96 95Q101 91 106 87Q111 84 116 81Q121 78 127 75Q132 72 138 70Q144 68 149 66Q155 64 161 63Q167 61 173 60Q179 59 185 59Q190 59 196 59Z";

/// The parts both pages share: fonts, colours, the top line and the column.
const PAGE_HEAD: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{title}} · Oak</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Geist+Mono:wght@400;600&family=Yarndings+20&display=swap" rel="stylesheet">
<style>
:root{
  color-scheme:light dark;
  --paper:255 255 255;--ink:10 10 10;--soft:74 74 74;--line:184 184 184;
  --mono:'Geist Mono','JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,monospace;
  --yarn:'Yarndings 20',var(--mono);
}
@media(prefers-color-scheme:dark){:root{--paper:0 0 0;--ink:245 245 245;--soft:160 160 160;--line:90 90 90}}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;background:rgb(var(--paper));color:rgb(var(--ink));font-family:var(--mono);
  font-size:.8125rem;line-height:1.7;-webkit-font-smoothing:antialiased}
:focus-visible{outline:2px solid rgb(var(--ink));outline-offset:2px}
.top,.page{width:100%;margin-inline:auto;padding-inline:1.5rem}
.top{max-width:58rem;padding-block:clamp(1.25rem,4.5vh,3rem)}
.top svg{display:block;width:1.25rem;height:1.25rem}
.page{max-width:25rem;display:flex;flex-direction:column;gap:1.75rem;
  padding-top:clamp(1rem,9vh,5.5rem);padding-bottom:4rem;text-align:center}
.page>*{margin:0}
h1{font-size:inherit;font-weight:600}
.dim{color:rgb(var(--soft))}
code{font:inherit}
{{style}}
</style></head>
<body>
<svg style="position:absolute;width:0;height:0" aria-hidden="true"><symbol id="oak-mark" viewBox="0 0 512 512" fill="currentColor"><path d="{{mark}}"/></symbol></svg>
<div class="top"><svg role="img" aria-label="Oak"><use href="#oak-mark"/></svg></div>
"##;

const OK_STYLE: &str = r##".game{display:flex;flex-direction:column;gap:.9rem}
.board{display:grid;grid-template-columns:repeat(4,1fr);gap:.5rem}
.card{aspect-ratio:1;background:none;border:0;padding:0;perspective:680px;cursor:pointer;font:inherit;color:inherit}
.card[disabled]{cursor:default}
.inner{display:block;position:relative;width:100%;height:100%;transform-style:preserve-3d;
  transition:transform .42s cubic-bezier(.2,.7,.2,1)}
.card.flipped .inner{transform:rotateY(180deg)}
.face{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;
  border:1px solid rgb(var(--line));background:rgb(var(--paper));
  backface-visibility:hidden;-webkit-backface-visibility:hidden}
.back svg{width:1rem;height:1rem;color:rgb(var(--soft)/.45)}
.front{transform:rotateY(180deg);font-family:var(--yarn);font-size:clamp(1.5rem,7vw,2.1rem);line-height:1}
.card:not([disabled]):hover .back{border-color:rgb(var(--ink))}
.card:not([disabled]):hover .back svg{color:rgb(var(--ink))}
.card.matched .front{background:rgb(var(--ink));border-color:rgb(var(--ink));color:rgb(var(--paper))}
.card.matched .inner{animation:pop .45s ease}
@keyframes pop{0%{transform:rotateY(180deg) scale(1)}45%{transform:rotateY(180deg) scale(1.08)}
  100%{transform:rotateY(180deg) scale(1)}}
.status{display:flex;align-items:center;justify-content:space-between;gap:1rem;color:rgb(var(--soft))}
.counts{font-variant-numeric:tabular-nums}
.counts b{color:rgb(var(--ink));font-weight:600}
.btn{padding:.3rem .65rem;border:1px solid rgb(var(--ink));background:rgb(var(--paper));color:rgb(var(--ink));
  font:inherit;font-size:.75rem;font-weight:600;line-height:1.4;white-space:nowrap;cursor:pointer}
.btn:hover{background:rgb(var(--ink));color:rgb(var(--paper))}
.msg{min-height:1.7em;margin:0}
.msg.win{font-weight:600}
.msg .y{font-family:var(--yarn);font-weight:400}"##;

const OK_BODY: &str = r##"<main class="page">
  <h1>Signed in as {{username}}</h1>
  <p class="dim">You can close this tab and go back to your terminal, or match the pairs while you're here.</p>
  <div class="game">
    <div class="board" id="board" aria-label="Memory match"></div>
    <div class="status">
      <span class="counts">Moves <b id="moves">0</b> &middot; Pairs <b id="pairs">0</b>/8</span>
      <button class="btn" id="again" type="button">Plant again</button>
    </div>
    <p class="msg" id="msg" role="status"></p>
  </div>
</main>
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
      c.type="button";c.className="card";c.dataset.sym=sym;c.setAttribute("aria-label","Tile");
      c.innerHTML='<span class="inner"><span class="face back"><svg aria-hidden="true"><use href="#oak-mark"/></svg></span>'+
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
        msgEl.innerHTML='Garden complete in '+moves+' moves &nbsp;<span class="y">d</span>';
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

const FAIL_BODY: &str = r##"<main class="page">
  <h1>Authorization failed</h1>
  <p class="dim">Go back to your terminal and run <code>oak login</code> again.</p>
</main>
</body></html>"##;

fn page(title: &str, style: &str, body: &str) -> String {
    let mut html = PAGE_HEAD
        .replace("{{title}}", title)
        .replace("{{style}}", style)
        .replace("{{mark}}", OAK_MARK_PATH);
    html.push_str(body);
    html
}

/// The page for a finished login, naming the account it signed in to.
fn ok_page(username: &str) -> String {
    page(
        "Signed in",
        OK_STYLE,
        &OK_BODY.replace("{{username}}", &escape_html(username)),
    )
}

fn fail_page() -> String {
    page("Authorization failed", "", FAIL_BODY)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod page_tests {
    use super::*;

    #[test]
    fn ok_page_names_the_account_escaped() {
        let html = ok_page("<b>zach</b>");
        assert!(html.contains("Signed in as &lt;b&gt;zach&lt;/b&gt;"));
        assert!(!html.contains("{{"));
    }

    #[test]
    fn pages_draw_the_oak_mark() {
        for html in [ok_page("zach"), fail_page()] {
            assert!(html.contains(OAK_MARK_PATH));
            assert!(html.contains(r##"<use href="#oak-mark"/>"##));
        }
    }
}
