#![allow(dead_code, mutable_transmutes, non_camel_case_types, non_snake_case, non_upper_case_globals, unused_assignments, unused_mut)]
#![feature(extern_types)]
extern "C" {
    pub type load_info;
    pub type module;
}
#[no_mangle]
pub unsafe extern "C" fn kmemleak_load_module(
    mut mod_0: *const module,
    mut info: *const load_info,
) {}
