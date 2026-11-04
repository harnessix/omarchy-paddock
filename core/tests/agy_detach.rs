//! The agy profile starts the terminal UI for a detached run.

#[test]
fn a_detached_agy_run_starts_the_terminal_ui_with_the_task() {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/profiles/agy.toml")).unwrap();
    let v: toml::Value = toml::from_str(&text).unwrap();
    let args: Vec<&str> = v["detach"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(
        args[0], "-i",
        "-i starts the terminal UI with a first prompt"
    );
    assert_eq!(args.len(), 2);
    assert!(args[1].contains("{prompt_file}"));
}
