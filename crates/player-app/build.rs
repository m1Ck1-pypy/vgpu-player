//! Встраивает иконку приложения в exe на Windows.
//!
//! На других платформах `embed-resource` ничего не делает, поэтому CI на Linux
//! собирается как раньше.

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=../../assets/app-icon.ico");

    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("не удалось встроить ресурсы приложения");
}
