fn main() {
    // The executable gets the Open Download Manager icon on Windows, so the file in Explorer and the
    // task bar look like an application rather than an anonymous binary. Other
    // platforms take their icon from the desktop file, and macOS from the bundle.
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/odm.ico");
        res.set("FileDescription", "Open Download Manager");
        res.set("ProductName", "Open Download Manager");
        res.set("CompanyName", "Open Download Manager");
        res.compile().unwrap();
    }
}
