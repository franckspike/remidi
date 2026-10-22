fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/remidi.ico");
        res.compile().unwrap();
    }
}
