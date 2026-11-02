use std::time::Duration;

use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use url::Url;

const MAX_REQUEST_BYTES: usize = 16 * 1024;

pub struct LoopbackCallback {
    listener: TcpListener,
    redirect: Url,
    /// The application's name, for the page the browser shows afterwards.
    app_name: &'static str,
}

impl LoopbackCallback {
    pub async fn bind(redirect_uri: &str, app_name: &'static str) -> Result<Self> {
        let redirect = Url::parse(redirect_uri).context("EVE redirect URI is invalid")?;
        if redirect.scheme() != "http" || redirect.host_str() != Some("127.0.0.1") {
            bail!("EVE redirect URI must use the IPv4 loopback address");
        }
        let port = redirect
            .port()
            .context("EVE redirect URI must have an explicit port")?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("could not listen for EVE SSO on 127.0.0.1:{port}"))?;
        Ok(Self {
            listener,
            redirect,
            app_name,
        })
    }

    pub async fn receive(
        &self,
        expected_state: &SecretString,
        wait: Duration,
    ) -> Result<SecretString> {
        let (mut stream, _) = timeout(wait, self.listener.accept())
            .await
            .context("timed out waiting for EVE SSO authorization")?
            .context("could not accept the EVE SSO callback")?;

        match read_callback(&mut stream, &self.redirect, expected_state).await {
            Ok(code) => {
                respond(
                    &mut stream,
                    "200 OK",
                    self.app_name,
                    "received the authorization. You can close this tab.",
                )
                .await?;
                Ok(code)
            }
            Err(error) => {
                let _ = respond(
                    &mut stream,
                    "400 Bad Request",
                    self.app_name,
                    "could not accept this authorization request. Return to it for details.",
                )
                .await;
                Err(error)
            }
        }
    }
}

async fn read_callback(
    stream: &mut TcpStream,
    redirect: &Url,
    expected_state: &SecretString,
) -> Result<SecretString> {
    let mut request = Vec::with_capacity(1024);
    loop {
        if request.len() == MAX_REQUEST_BYTES {
            bail!("EVE SSO callback request was too large");
        }
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_REQUEST_BYTES - request.len();
        request.extend_from_slice(&buffer[..read.min(remaining)]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let request = std::str::from_utf8(&request).context("EVE SSO callback was not valid HTTP")?;
    let first_line = request
        .lines()
        .next()
        .context("EVE SSO callback was empty")?;
    let mut parts = first_line.split_whitespace();
    if parts.next() != Some("GET") {
        bail!("EVE SSO callback did not use GET");
    }
    let target = parts
        .next()
        .context("EVE SSO callback had no request target")?;
    let received = redirect
        .join(target)
        .context("EVE SSO callback contained an invalid request target")?;
    if received.path() != redirect.path() {
        bail!("EVE SSO callback used the wrong path");
    }

    let mut code = None;
    let mut state = None;
    let mut oauth_error = None;
    for (key, value) in received.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => oauth_error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = oauth_error {
        bail!("EVE authorization was declined: {error}");
    }
    if state.as_deref() != Some(expected_state.expose_secret()) {
        bail!("EVE SSO callback state did not match");
    }
    let code = code.context("EVE SSO callback did not include an authorization code")?;
    Ok(SecretString::from(code))
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    app_name: &str,
    message: &str,
) -> Result<()> {
    let message = format!("{app_name} {message}");
    let body =
        format!("<!doctype html><meta charset=utf-8><title>{app_name}</title><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_a_matching_callback_without_exposing_the_code() {
        let redirect: Url = "http://127.0.0.1:17632/callback".parse().unwrap();
        let expected = SecretString::from("expected-state".to_owned());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(
                    b"GET /callback?code=private-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();

        let code = read_callback(&mut stream, &redirect, &expected)
            .await
            .unwrap();
        assert_eq!(code.expose_secret(), "private-code");
        client.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_a_mismatched_state() {
        let redirect: Url = "http://127.0.0.1:17632/callback".parse().unwrap();
        let expected = SecretString::from("expected-state".to_owned());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(
                    b"GET /callback?code=private-code&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();

        assert!(
            read_callback(&mut stream, &redirect, &expected)
                .await
                .is_err()
        );
    }
}
