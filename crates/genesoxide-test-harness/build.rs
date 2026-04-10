use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));
    let ymfm_src = manifest_dir
        .join("..")
        .join("..")
        .join("third_party")
        .join("ymfm-main")
        .join("src");
    let bridge = manifest_dir.join("native").join("ymfm_bridge.cpp");

    println!("cargo:rerun-if-changed={}", bridge.display());
    println!("cargo:rerun-if-changed={}", ymfm_src.display());

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .include(&ymfm_src)
        .file(&bridge)
        .file(ymfm_src.join("ymfm_adpcm.cpp"))
        .file(ymfm_src.join("ymfm_misc.cpp"))
        .file(ymfm_src.join("ymfm_opl.cpp"))
        .file(ymfm_src.join("ymfm_opm.cpp"))
        .file(ymfm_src.join("ymfm_opn.cpp"))
        .file(ymfm_src.join("ymfm_opq.cpp"))
        .file(ymfm_src.join("ymfm_opz.cpp"))
        .file(ymfm_src.join("ymfm_pcm.cpp"))
        .file(ymfm_src.join("ymfm_ssg.cpp"))
        .flag_if_supported("-std=c++17")
        .flag_if_supported("/std:c++17")
        .warnings(false);

    build.compile("ymfm_bridge");
}
