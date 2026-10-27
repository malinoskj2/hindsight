//! HTTPS downloads through `WinHTTP`, which uses the system's certificate store and proxy settings
//! and follows redirects, except from HTTPS to HTTP.

use std::ffi::c_void;
use thiserror::Error;
use windows::{
    Win32::Networking::WinHttp::{
        INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_REFRESH,
        WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    },
    core::{HSTRING, PCWSTR, w},
};

const SCHEME: &str = "https://";
const STATUS_OK: u32 = 200;
const RESOLVE_TIMEOUT_MS: i32 = 10_000;
const CONNECT_TIMEOUT_MS: i32 = 10_000;
const SEND_TIMEOUT_MS: i32 = 30_000;
const RECEIVE_TIMEOUT_MS: i32 = 30_000;
const CHUNK: usize = 64 << 10;
const ERROR_WINHTTP_TIMEOUT: u32 = 12_002;
const ERROR_WINHTTP_NAME_NOT_RESOLVED: u32 = 12_007;
const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12_029;
const ERROR_WINHTTP_CONNECTION_ERROR: u32 = 12_030;
const ERROR_WINHTTP_SECURE_FAILURE: u32 = 12_175;

#[derive(Debug, Error)]
pub(crate) enum HttpError {
    #[error("{0} is not an HTTPS address")]
    NotHttps(String),
    #[error("{}", network_failure(.0))]
    Network(windows::core::Error),
    #[error("the server answered {0}")]
    Status(u32),
    #[error("the download is larger than {0} bytes")]
    TooLarge(usize),
}

impl From<windows::core::Error> for HttpError {
    fn from(error: windows::core::Error) -> Self {
        Self::Network(error)
    }
}

/// Fetches `url`, an `https://` address, refusing a body larger than `limit` bytes.
pub(crate) fn get(url: &str, limit: usize) -> Result<Vec<u8>, HttpError> {
    let (host, path) = split(url).ok_or_else(|| HttpError::NotHttps(url.to_owned()))?;
    let agent = HSTRING::from(concat!("hindsight/", env!("CARGO_PKG_VERSION")));

    // Declared parent first, so each handle closes before the one it was opened from.
    // SAFETY: the agent string outlives the call, and the null proxy names select none.
    let session = Handle::new(unsafe {
        WinHttpOpen(
            &agent,
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    })?;
    // SAFETY: the session handle is open.
    unsafe {
        WinHttpSetTimeouts(
            session.0,
            RESOLVE_TIMEOUT_MS,
            CONNECT_TIMEOUT_MS,
            SEND_TIMEOUT_MS,
            RECEIVE_TIMEOUT_MS,
        )
    }?;
    // SAFETY: the session handle is open and the host string outlives the call.
    let connection = Handle::new(unsafe {
        WinHttpConnect(
            session.0,
            &HSTRING::from(host),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        )
    })?;
    // SAFETY: the connection handle is open, the strings outlive the call, and the null
    // version, referrer and accept types select the defaults.
    let request = Handle::new(unsafe {
        WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE | WINHTTP_FLAG_REFRESH,
        )
    })?;
    // SAFETY: the request handle is open, and there are no extra headers or body.
    unsafe { WinHttpSendRequest(request.0, None, None, 0, 0, 0) }?;
    // SAFETY: the request handle is open and has been sent.
    unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }?;

    let status = request.status()?;
    if status != STATUS_OK {
        return Err(HttpError::Status(status));
    }
    request.read_body(limit)
}

/// Splits an `https://` address into its host and the path with any query, which starts with
/// `/`.
fn split(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix(SCHEME)?;
    let (host, path) = rest.find('/').map_or((rest, "/"), |at| rest.split_at(at));
    (!host.is_empty()).then_some((host, path))
}

fn network_failure(error: &windows::core::Error) -> String {
    let reason = match error.code().0.cast_unsigned() & 0xFFFF {
        ERROR_WINHTTP_TIMEOUT => "the connection timed out",
        ERROR_WINHTTP_NAME_NOT_RESOLVED => "the server could not be found",
        ERROR_WINHTTP_CANNOT_CONNECT | ERROR_WINHTTP_CONNECTION_ERROR => {
            "the server could not be reached"
        }
        ERROR_WINHTTP_SECURE_FAILURE => "the secure connection could not be established",
        _ => return format!("the download failed: {error}"),
    };
    reason.to_owned()
}

/// An open `WinHTTP` handle, closed on drop.
struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> windows::core::Result<Self> {
        if raw.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self(raw))
    }

    fn status(&self) -> windows::core::Result<u32> {
        let mut status = 0u32;
        let mut size = super::win32::cb_size::<u32>();
        // SAFETY: the request has its response, and the buffer is a `u32` of the size given,
        // which is what a numeric query writes.
        unsafe {
            WinHttpQueryHeaders(
                self.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut status).cast()),
                &raw mut size,
                std::ptr::null_mut(),
            )
        }?;
        Ok(status)
    }

    fn read_body(&self, limit: usize) -> Result<Vec<u8>, HttpError> {
        let mut body = Vec::new();
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let mut read = 0u32;
            // SAFETY: the request has its response, and the buffer is `CHUNK` bytes long.
            unsafe {
                WinHttpReadData(
                    self.0,
                    chunk.as_mut_ptr().cast(),
                    super::win32::cb_size::<[u8; CHUNK]>(),
                    &raw mut read,
                )
            }?;
            let Some(read) = chunk.get(..read as usize).filter(|read| !read.is_empty()) else {
                return Ok(body);
            };
            if body.len() + read.len() > limit {
                return Err(HttpError::TooLarge(limit));
            }
            body.extend_from_slice(read);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is open and owned here, and every handle opened from it has been
        // closed first.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_addresses_are_split() {
        assert_eq!(
            split("https://github.com/malinoskj2/hindsight/releases?x=1"),
            Some(("github.com", "/malinoskj2/hindsight/releases?x=1"))
        );
        assert_eq!(split("https://github.com"), Some(("github.com", "/")));
        assert_eq!(split("http://github.com/"), None);
        assert_eq!(split("https:///path"), None);
    }

    #[test]
    fn known_network_failures_are_named() {
        let timeout = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
            ERROR_WINHTTP_TIMEOUT,
        ));
        assert_eq!(network_failure(&timeout), "the connection timed out");
    }

    #[test]
    #[ignore = "reaches GitHub; run by hand"]
    fn by_hand_the_latest_release_signature_downloads() {
        let body = get(&crate::update::latest_signature_url(), 4 << 10).unwrap();
        assert!(body.starts_with(b"untrusted comment:"));
    }
}
