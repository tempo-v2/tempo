use anyhow::{Context, Result, bail, ensure};
use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{
    env::{VarError, var},
    net::Ipv4Addr,
    path::Path,
};
use tokio::net::TcpListener;

#[derive(Clone)]
pub(super) struct Access {
    authority: HeaderValue,
    origin: HeaderValue,
    login: HeaderValue,
}

impl Access {
    fn from_environment() -> Result<Option<Self>> {
        match (
            var("TEMPO_LEAN_WEB_ORIGIN"),
            var("TEMPO_LEAN_WEB_TAILSCALE_LOGIN"),
        ) {
            (Err(VarError::NotPresent), Err(VarError::NotPresent)) => Ok(None),
            (Ok(origin), Ok(login)) => Ok(Some(Self::new(&origin, &login)?)),
            _ => bail!("set both TEMPO_LEAN_WEB_ORIGIN and TEMPO_LEAN_WEB_TAILSCALE_LOGIN"),
        }
    }

    pub(super) fn new(origin: &str, login: &str) -> Result<Self> {
        let uri: Uri = origin.parse()?;
        let valid_authority = uri.authority().filter(|authority| {
            authority.host().ends_with(".ts.net")
                && authority
                    .port_u16()
                    .is_some_and(|port| !matches!(port, 0 | 443))
                && origin == format!("https://{authority}")
                && !authority.as_str().contains('@')
        });
        let Some(authority) = valid_authority else {
            bail!(
                "expected a Tailscale HTTPS origin with a .ts.net host and explicit non-default port"
            );
        };
        if login.is_empty() || login.bytes().any(|byte| byte.is_ascii_whitespace()) {
            bail!("expected one Tailscale owner login");
        }
        Ok(Self {
            authority: authority.as_str().parse()?,
            origin: origin.parse()?,
            login: login.parse()?,
        })
    }

    fn permits(&self, method: &Method, headers: &HeaderMap) -> bool {
        let exact = |key, expected: &HeaderValue| {
            let mut values = headers.get_all(key).iter();
            values.next() == Some(expected) && values.next().is_none()
        };
        let same_origin = exact("origin", &self.origin);
        let read = matches!(*method, Method::GET | Method::HEAD);
        exact("host", &self.authority)
            && exact("x-forwarded-host", &self.authority)
            && exact("x-forwarded-proto", &HeaderValue::from_static("https"))
            && exact("tailscale-user-login", &self.login)
            && !headers.contains_key("tailscale-funnel-request")
            && ((read && (same_origin || !headers.contains_key(header::ORIGIN)))
                || (*method == Method::POST && same_origin))
    }
}

pub(super) async fn admit(State(access): State<Access>, req: Request, next: Next) -> Response {
    let mut reply = if access.permits(req.method(), req.headers()) {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    };
    reply
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    reply
}

pub(super) async fn listen(path: &Path) -> Result<Option<(TcpListener, Access)>> {
    let Some(access) = Access::from_environment()? else {
        return Ok(None);
    };
    ensure!(
        path.join("private.html").is_file(),
        "build the private UI first"
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 41824))
        .await
        .context("bind the loopback listener on port 41824")?;
    Ok(Some((listener, access)))
}
