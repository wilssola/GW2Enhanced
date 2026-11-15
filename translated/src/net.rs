//! HTTPS through WinHTTP (part of Windows: no TLS library in the DLL).
//!
//! Only the project's server ([`HOST`]) is ever called, over HTTPS, and redirects are never followed, so a request
//! cannot end up on another host.

use std::ffi::c_void;
use std::io;

use windows_sys::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WinHttpQueryDataAvailable, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};

/// The project's server (`backend/`).
pub const HOST: &str = "gw2enhanced.wilssola.dev";

/// Largest response body kept for a POST (bytes).
const MAX_RESPONSE: usize = 64 * 1024;

/// Timeouts (ms): resolve, connect, send, receive.
const TIMEOUTS: (i32, i32, i32, i32) = (10_000, 10_000, 30_000, 30_000);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Closes a WinHTTP handle when dropped.
struct Handle(*mut c_void);

impl Handle {
    fn new(h: *mut c_void) -> io::Result<Self> {
        if h.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `POST https://{host}{path}` with a JSON body; returns the HTTP status and
/// the start of the response body.
pub fn post_json(host: &str, path: &str, body: &[u8]) -> io::Result<(u32, Vec<u8>)> {
    let headers = "Content-Type: application/json\r\n";
    let (status, mut response) = request(host, "POST", path, headers, body, MAX_RESPONSE)?;
    response.truncate(MAX_RESPONSE);
    Ok((status, response))
}

/// `GET https://{host}{path}`; returns the HTTP status and the body. A body longer than `max` bytes is an error
/// (nothing past `max + 1` bytes is read).
pub fn get(host: &str, path: &str, max: usize) -> io::Result<(u32, Vec<u8>)> {
    let (status, response) = request(host, "GET", path, "", &[], max)?;
    if response.len() > max {
        return Err(io::Error::other(format!("response larger than {max} bytes")));
    }
    Ok((status, response))
}

/// One HTTPS request; reads at most `max + 1` bytes of the body.
fn request(host: &str, verb: &str, path: &str, headers: &str, body: &[u8], max: usize) -> io::Result<(u32, Vec<u8>)> {
    let agent = wide(&format!("GW2Translated/{}", crate::VERSION));
    let (host, path, verb) = (wide(host), wide(path), wide(verb));
    let headers = wide(headers);
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("body too large"))?;
    let limit = max.saturating_add(1);
    unsafe {
        let session = Handle::new(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ))?;
        let (r, c, s, v) = TIMEOUTS;
        check(WinHttpSetTimeouts(session.0, r, c, s, v))?;
        let connection = Handle::new(WinHttpConnect(session.0, host.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0))?;
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let never = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        check(WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&raw const never).cast(),
            std::mem::size_of::<u32>() as u32,
        ))?;
        let (header_ptr, header_len) = if headers.len() > 1 {
            (headers.as_ptr(), u32::MAX)
        } else {
            (std::ptr::null(), 0)
        };
        let body_ptr = if body.is_empty() { std::ptr::null() } else { body.as_ptr().cast() };
        check(WinHttpSendRequest(request.0, header_ptr, header_len, body_ptr, len, len, 0))?;
        check(WinHttpReceiveResponse(request.0, std::ptr::null_mut()))?;
        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        check(WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&raw mut status).cast(),
            &mut size,
            std::ptr::null_mut(),
        ))?;
        let mut response = Vec::new();
        loop {
            let mut available: u32 = 0;
            check(WinHttpQueryDataAvailable(request.0, &mut available))?;
            if available == 0 || response.len() >= limit {
                break;
            }
            let start = response.len();
            let want = (available as usize).min(limit - start);
            response.resize(start + want, 0);
            let mut read: u32 = 0;
            check(WinHttpReadData(
                request.0,
                response[start..].as_mut_ptr().cast(),
                want as u32,
                &mut read,
            ))?;
            response.truncate(start + read as usize);
            if read == 0 {
                break;
            }
        }
        Ok((status, response))
    }
}
