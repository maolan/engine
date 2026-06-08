use nix::libc;

pub(super) fn read_nonblock(
    fd: i32,
    dst: &mut [u8],
    len: usize,
    max_chunk: usize,
    count: &mut usize,
) -> std::io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let mut offset = 0;
    let max_chunk = max_chunk.max(1);
    while offset < len {
        let take = (len - offset).min(max_chunk);
        let n = unsafe { libc::read(fd, dst[offset..].as_ptr() as *mut libc::c_void, take) };
        if n >= 0 {
            let n = n as usize;
            *count += n;
            offset += n;
            if n < take {
                return Ok(());
            }
            continue;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

pub(super) fn write_nonblock(
    fd: i32,
    src: &mut [u8],
    len: usize,
    max_chunk: usize,
    count: &mut usize,
) -> std::io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let mut offset = 0;
    let max_chunk = max_chunk.max(1);
    while offset < len {
        let take = (len - offset).min(max_chunk);
        let n = unsafe { libc::write(fd, src[offset..].as_ptr() as *const libc::c_void, take) };
        if n >= 0 {
            let n = n as usize;
            *count += n;
            offset += n;
            if n < take {
                return Ok(());
            }
            continue;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

pub(super) fn map_read(
    map: *mut libc::c_void,
    mapped: bool,
    total: usize,
    dst: &mut [u8],
    mut offset: usize,
    mut length: usize,
) -> usize {
    if !mapped || map.is_null() || length == 0 || total == 0 {
        return 0;
    }
    offset %= total;
    if length > total {
        length = total;
    }
    let mut copied = 0;
    while length > 0 {
        let take = (total - offset).min(length);
        unsafe {
            std::ptr::copy_nonoverlapping(
                (map as *const u8).add(offset),
                dst[copied..].as_ptr() as *mut u8,
                take,
            );
        }
        copied += take;
        length -= take;
        offset = 0;
    }
    copied
}

pub(super) fn map_write(
    map: *mut libc::c_void,
    mapped: bool,
    total: usize,
    src: Option<&mut [u8]>,
    mut offset: usize,
    mut length: usize,
) -> usize {
    if !mapped || map.is_null() || length == 0 || total == 0 {
        return 0;
    }
    offset %= total;
    if length > total {
        length = total;
    }
    let mut copied = 0;
    while length > 0 {
        let take = (total - offset).min(length);
        unsafe {
            if let Some(data) = src.as_ref() {
                std::ptr::copy_nonoverlapping(
                    data[copied..].as_ptr(),
                    (map as *mut u8).add(offset),
                    take,
                );
            } else {
                std::ptr::write_bytes((map as *mut u8).add(offset), 0, take);
            }
        }
        copied += take;
        length -= take;
        offset = 0;
    }
    copied
}
