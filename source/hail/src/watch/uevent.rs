/// True for a kernel uevent (`ACTION@DEVPATH\0KEY=VALUE\0...`) from the block subsystem.
pub fn is_block_uevent(message: &[u8]) -> bool {
    let mut fields = message.split(|&byte| byte == 0);
    let header_ok = fields
        .next()
        .is_some_and(|header| header.contains(&b'@') && !header.starts_with(b"libudev"));
    header_ok && fields.any(|field| field == b"SUBSYSTEM=block")
}
