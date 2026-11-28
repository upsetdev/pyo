//! Cookies: the ones a request sent, and the `Set-Cookie` headers its
//! response sends back. Pyo has one, the session (auth.rs).
//!
//! As an extractor, [`CookieJar`] reads the `Cookie` headers. Returned
//! with a response, it adds a `Set-Cookie` header for each cookie set or
//! cleared on it. A name sent more than once reads as its last value.

use std::convert::Infallible;

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, HeaderValue, header, request::Parts},
    response::{IntoResponseParts, ResponseParts},
};

/// The request's cookies, and the ones its response sets.
#[derive(Debug, Default)]
pub struct CookieJar {
    sent: Vec<(String, String)>,
    set: Vec<HeaderValue>,
}

/// A cookie for Pyo's own pages: sent with every path (`Path=/`), never
/// readable by scripts (`HttpOnly`), and left off requests other sites
/// start, except following a link (`SameSite=Lax`).
pub struct Cookie<'a> {
    pub name: &'a str,
    pub value: &'a str,
    /// Seconds it lasts. 0 clears it.
    pub max_age: i64,
    /// Sent over HTTPS only.
    pub secure: bool,
}

impl CookieJar {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let sent = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                let name = name.trim();
                (!name.is_empty()).then(|| (name.to_owned(), value.trim().to_owned()))
            })
            .collect();
        Self {
            sent,
            set: Vec::new(),
        }
    }

    /// What the request sent for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.sent
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Sets `cookie` on the response. Fails if its name or value has a
    /// character a cookie can't hold.
    pub fn add(mut self, cookie: Cookie<'_>) -> anyhow::Result<Self> {
        self.set.push(set_cookie(&cookie)?);
        Ok(self)
    }

    /// Clears the cookie `name` on the response.
    pub fn remove(self, name: &str, secure: bool) -> anyhow::Result<Self> {
        self.add(Cookie {
            name,
            value: "",
            max_age: 0,
            secure,
        })
    }
}

/// RFC 6265's cookie-octet: printable ASCII but `"`, `,`, `;` and `\`.
fn octets(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b',' | b';' | b'\\'))
}

fn set_cookie(c: &Cookie<'_>) -> anyhow::Result<HeaderValue> {
    anyhow::ensure!(
        !c.name.is_empty() && octets(c.name) && !c.name.contains('=') && octets(c.value),
        "cookie {:?} has a character a cookie can't hold",
        c.name
    );
    let mut v = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        c.name,
        c.value,
        c.max_age.max(0)
    );
    if c.secure {
        v.push_str("; Secure");
    }
    Ok(HeaderValue::from_str(&v)?)
}

impl<S: Send + Sync> FromRequestParts<S> for CookieJar {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Self::from_headers(&parts.headers))
    }
}

impl IntoResponseParts for CookieJar {
    type Error = Infallible;

    fn into_response_parts(self, mut res: ResponseParts) -> Result<ResponseParts, Infallible> {
        for v in self.set {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn sent(values: &[&str]) -> CookieJar {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(header::COOKIE, HeaderValue::from_str(v).unwrap());
        }
        CookieJar::from_headers(&h)
    }

    #[test]
    fn reads_what_the_browser_sent() {
        let jar = sent(&["theme=dark; pyo_session=ab12", " other = x "]);
        assert_eq!(jar.get("pyo_session"), Some("ab12"));
        assert_eq!(jar.get("other"), Some("x"));
        assert_eq!(jar.get("theme"), Some("dark"));
        assert_eq!(jar.get("missing"), None);
        // A value may hold '='; the last of a repeated name counts.
        let jar = sent(&["a=b=c; pyo_session=old; pyo_session=new"]);
        assert_eq!(jar.get("a"), Some("b=c"));
        assert_eq!(jar.get("pyo_session"), Some("new"));
        // Junk is skipped, not an error.
        let jar = sent(&[";;=x; noequals; pyo_session="]);
        assert_eq!(jar.get("pyo_session"), Some(""));
        assert_eq!(jar.get(""), None);
        assert_eq!(sent(&[]).get("pyo_session"), None);
    }

    #[test]
    fn sets_and_clears_with_its_attributes() {
        let jar = CookieJar::default()
            .add(Cookie {
                name: "pyo_session",
                value: "ab12",
                max_age: 3600,
                secure: true,
            })
            .unwrap()
            .remove("old", false)
            .unwrap();
        let res = (jar, "ok").into_response();
        let set: Vec<_> = res
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            set,
            [
                "pyo_session=ab12; HttpOnly; SameSite=Lax; Path=/; Max-Age=3600; Secure",
                "old=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
            ]
        );
    }

    #[test]
    fn refuses_what_a_cookie_cannot_hold() {
        for (name, value) in [
            ("pyo_session", "a;b"),
            ("pyo_session", "a b"),
            ("pyo_session", "a\"b"),
            ("pyo_session", "a\nb"),
            ("", "x"),
            ("a=b", "x"),
        ] {
            let c = Cookie {
                name,
                value,
                max_age: 1,
                secure: false,
            };
            assert!(CookieJar::default().add(c).is_err(), "{name:?}={value:?}");
        }
    }
}
