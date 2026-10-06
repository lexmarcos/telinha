//! Compila a interface e diz onde o binário procura o FFmpeg e a libva que
//! vão junto com ele.

fn main() {
    // A interface (ui/app.slint e o que ela importa).
    slint_build::compile("ui/app.slint").expect("compilar a interface");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        // Pacotes: bibliotecas em lib/ ao lado do binário (AppImage, portátil)
        // ou em /usr/lib/telinha (.deb).
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/lib:$ORIGIN/../lib/telinha");
        // Desenvolvimento: as cópias baixadas em .deps.
        if std::env::var("PROFILE").as_deref() == Ok("debug") || std::env::var("TELINHA_DEV_RPATH").is_ok() {
            let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}/.deps/libva/lib:{dir}/.deps/ffmpeg-linux/lib");
        }
    }
    if target_os == "windows" {
        // Ícone e informações de versão do .exe.
        embed_resource::compile("assets/windows/telinha.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
    println!("cargo:rerun-if-env-changed=TELINHA_DEV_RPATH");
    // Servidor padrão embutido (option_env!): trocar a variável recompila.
    println!("cargo:rerun-if-env-changed=TELINHA_SERVER");
    println!("cargo:rerun-if-env-changed=TELINHA_DISCORD_ID");
}
