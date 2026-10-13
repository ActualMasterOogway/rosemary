mod common;

const INSERTED: &str = r#"
extern int puts(const char *);
__attribute__((constructor)) static void init(void) { puts("inserted"); }
"#;

const MAIN: &str = r#"
extern int puts(const char *);
int main(void) { puts("main"); return 0; }
"#;

#[test]
fn host_variable_inserts_a_library() {
    let Some(lib) = common::build_x86_64("insert_lib", INSERTED, &["-dynamiclib"]) else {
        return;
    };
    let Some(main) = common::build_x86_64("insert_main", MAIN, &[]) else {
        return;
    };
    let (_, out, _) =
        common::run_guest_env(&[("ROSEMARY_DYLD_INSERT_LIBRARIES", &lib)], &main, &[]);
    assert_eq!(out, "inserted\nmain\n");
}

#[test]
fn guest_variable_is_the_one_that_counts() {
    let Some(lib) = common::build_x86_64("insert_lib", INSERTED, &["-dynamiclib"]) else {
        return;
    };
    let Some(main) = common::build_x86_64("insert_main", MAIN, &[]) else {
        return;
    };
    // The host's spelling is not the guest's, so it inserts nothing.
    let (_, out, _) = common::run_guest_env(&[("DYLD_INSERT_LIBRARIES", &lib)], &main, &[]);
    assert_eq!(out, "main\n");
    let line = format!("DYLD_INSERT_LIBRARIES={lib}");
    let (_, out, _) = common::run_guest_env(&[("ROSEMARY_ENVIRON", &line)], &main, &[]);
    assert_eq!(out, "inserted\nmain\n");
}
