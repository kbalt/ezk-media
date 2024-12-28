use std::env;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(openssl320)");

    let v = env::var("DEP_OPENSSL_VERSION_NUMBER").unwrap();
    
    let version = u64::from_str_radix(&v, 16).unwrap();

    #[allow(clippy::unusual_byte_groupings)]
    if version >= 0x30200000 {
        println!("cargo:rustc-cfg=openssl320");
    }
}
