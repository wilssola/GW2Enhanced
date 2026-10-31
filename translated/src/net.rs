//! HTTPS through WinHTTP (part of Windows: no TLS library in the DLL).

use std::ffi::c_void;
use std::io;

use windows_sys::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetTimeouts,
};

/// Largest response body kept (bytes).
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
    let agent = wide(&format!("GW2Translated/{}", crate::VERSION));
    let (host, path, verb) = (wide(host), wide(path), wide("POST"));
    let headers = wide("Content-Type: application/json\r\n");
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("body too large"))?;
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
        check(WinHttpSendRequest(
            request.0,
            headers.as_ptr(),
            u32::MAX,
            body.as_ptr().cast(),
            len,
            len,
            0,
        ))?;
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
            if available == 0 || response.len() >= MAX_RESPONSE {
                break;
            }
            let start = response.len();
            response.resize(start + available as usize, 0);
            let mut read: u32 = 0;
            check(WinHttpReadData(
                request.0,
                response[start..].as_mut_ptr().cast(),
                available,
                &mut read,
            ))?;
            response.truncate(start + read as usize);
            if read == 0 {
                break;
            }
        }
        response.truncate(MAX_RESPONSE);
        Ok((status, response))
    }
}
