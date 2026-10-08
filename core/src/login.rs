//! Nextcloud Login Flow v2: the user signs in in a browser (works behind SSO), we get an app password.

use crate::json::Json;
use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct LoginFlow {
    /// Open this in a browser.
    pub login_url: String,
    pub endpoint: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub server: String,
    pub login_name: String,
    pub app_password: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent("Todav")
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent()
}

fn post(url: &str, form: &[(&str, &str)]) -> Result<(u16, Option<Json>)> {
    let mut r = agent()
        .post(url)
        .send_form(form.iter().copied())
        .map_err(|e| Error::Net(e.to_string()))?;
    let body = r
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Net(e.to_string()))?;
    Ok((r.status().as_u16(), Json::parse(&body)))
}

pub fn login_flow_start(server: String) -> Result<LoginFlow> {
    let server = server.trim().trim_end_matches('/');
    let server = if server.starts_with("http") {
        server.to_string()
    } else {
        format!("https://{server}")
    };
    let (status, json) = post(&format!("{server}/index.php/login/v2"), &[])?;
    let field = |j: &Json, k: &str| j.get(k).and_then(Json::str).map(str::to_string);
    let bad = || Error::Http(status, "not a Nextcloud login flow response".into());
    let json = json.filter(|_| status == 200).ok_or_else(bad)?;
    let poll = json.get("poll").ok_or_else(bad)?;
    Ok(LoginFlow {
        login_url: field(&json, "login").ok_or_else(bad)?,
        endpoint: field(poll, "endpoint").ok_or_else(bad)?,
        token: field(poll, "token").ok_or_else(bad)?,
    })
}

/// None while the user has not finished signing in (poll every few seconds; the flow expires after 20 min).
pub fn login_flow_poll(flow: &LoginFlow) -> Result<Option<Credentials>> {
    let (status, json) = post(&flow.endpoint, &[("token", &flow.token)])?;
    if status == 404 {
        return Ok(None);
    }
    let field = |k: &str| json.as_ref()?.get(k)?.str().map(str::to_string);
    match (
        status,
        field("server"),
        field("loginName"),
        field("appPassword"),
    ) {
        (200, Some(server), Some(login_name), Some(app_password)) => Ok(Some(Credentials {
            server,
            login_name,
            app_password,
        })),
        _ => Err(Error::Http(status, "login flow failed".into())),
    }
}
