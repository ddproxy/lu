//! This file is its own test binary: changing `PATH` here cannot affect other tests.

#[cfg(unix)]
#[test]
fn disk_space_works_without_df_on_path() {
    use std::path::Path;

    std::env::set_var("PATH", "");
    let space = tuishou_ting::housekeeping::disk_space(Path::new("/tmp")).unwrap();
    assert!(space.total_bytes > 0);
    assert!(space.free_bytes <= space.total_bytes);
}
