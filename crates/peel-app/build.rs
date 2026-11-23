//! Windows では exe に桃のアイコンを埋め込み、エクスプローラーや
//! ショートカットでもアイコンが出るようにする。

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons/peachey.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icons/peachey.ico");
        if let Err(e) = res.compile() {
            // アイコンが埋め込めなくてもアプリは動くので、ビルドは止めない。
            println!("cargo:warning=exe にアイコンを埋め込めませんでした: {e}");
        }
    }
}
