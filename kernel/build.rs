// Compiles the freestanding C sources under `csrc/` and links the result
// straight into the kernel binary -- the "wire a C toolchain into the
// build" half of the DOOM-porting groundwork (see the top-level README's
// roadmap). This is a normal `std` program: Cargo always builds and runs
// build scripts for the *host*, never the `--target` triple a crate itself
// is being built for, so this can freely use `std::process::Command` even
// though the kernel crate it's building for is `#![no_std]`.
//
// Deliberately not using the `cc` crate (or any dependency at all): this
// only needs to invoke `clang` (or `cc`) with a handful of flags on one
// small source file today, and staying dependency-free means `cargo build`
// keeps working offline in a fresh environment, same reasoning as the
// stable-toolchain trick documented in `Cargo.toml`/`.cargo/config.toml`.
//
// The flags mirror the Rust side's own freestanding setup (see
// `.cargo/config.toml`) as closely as C lets them: no red zone (interrupts
// can land mid-function), the `kernel` code model (this links into a
// binary based at the top 2 GiB of the address space, same reason
// `code-model=kernel` exists on the Rust side), no stack protector (no
// `__stack_chk_fail` implemented -- there's nothing for it to call), no
// PIC/PIE (this is a flat, absolutely-linked kernel), and no standard
// headers or library assumptions at all (`-ffreestanding`). Calls this
// code makes to things like `memcpy`/`memset` resolve against
// `intrinsics.rs`'s existing `#[no_mangle]` definitions -- the same ones
// `compiler_builtins` already relies on -- and calls to `malloc`/`free`
// resolve against `libc_shim.rs`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// C sources to compile and archive into `libcdoom.a`. Add to this list as
/// more of doomgeneric's own sources get pulled in later; for now it's a
/// handful of self-contained demos proving each piece of the pipeline
/// (malloc/free, printf, file I/O) works.
const C_SOURCES: &[&str] = &[
    "csrc/cdemo.c",
    "csrc/printf.c",
    "csrc/iodemo.c",
    "csrc/sscanf.c",
    "csrc/doom_math.c",
    "csrc/doom_string.c",
    "csrc/doom/am_map.c",
    "csrc/doom/d_event.c",
    "csrc/doom/d_items.c",
    "csrc/doom/d_iwad.c",
    "csrc/doom/d_loop.c",
    "csrc/doom/d_main.c",
    "csrc/doom/d_mode.c",
    "csrc/doom/d_net.c",
    "csrc/doom/doomdef.c",
    "csrc/doom/doomgeneric.c",
    "csrc/doom/doomstat.c",
    "csrc/doom/dstrings.c",
    "csrc/doom/dummy.c",
    "csrc/doom/f_finale.c",
    "csrc/doom/f_wipe.c",
    "csrc/doom/g_game.c",
    "csrc/doom/hu_lib.c",
    "csrc/doom/hu_stuff.c",
    "csrc/doom/i_cdmus.c",
    "csrc/doom/i_endoom.c",
    "csrc/doom/i_input.c",
    "csrc/doom/i_joystick.c",
    "csrc/doom/i_scale.c",
    "csrc/doom/i_sound.c",
    "csrc/doom/i_system.c",
    "csrc/doom/i_timer.c",
    "csrc/doom/i_video.c",
    "csrc/doom/info.c",
    "csrc/doom/m_argv.c",
    "csrc/doom/m_bbox.c",
    "csrc/doom/m_cheat.c",
    "csrc/doom/m_config.c",
    "csrc/doom/m_controls.c",
    "csrc/doom/m_fixed.c",
    "csrc/doom/m_menu.c",
    "csrc/doom/m_misc.c",
    "csrc/doom/m_random.c",
    "csrc/doom/memio.c",
    "csrc/doom/mus2mid.c",
    "csrc/doom/p_ceilng.c",
    "csrc/doom/p_doors.c",
    "csrc/doom/p_enemy.c",
    "csrc/doom/p_floor.c",
    "csrc/doom/p_inter.c",
    "csrc/doom/p_lights.c",
    "csrc/doom/p_map.c",
    "csrc/doom/p_maputl.c",
    "csrc/doom/p_mobj.c",
    "csrc/doom/p_plats.c",
    "csrc/doom/p_pspr.c",
    "csrc/doom/p_saveg.c",
    "csrc/doom/p_setup.c",
    "csrc/doom/p_sight.c",
    "csrc/doom/p_spec.c",
    "csrc/doom/p_switch.c",
    "csrc/doom/p_telept.c",
    "csrc/doom/p_tick.c",
    "csrc/doom/p_user.c",
    "csrc/doom/r_bsp.c",
    "csrc/doom/r_data.c",
    "csrc/doom/r_draw.c",
    "csrc/doom/r_main.c",
    "csrc/doom/r_plane.c",
    "csrc/doom/r_segs.c",
    "csrc/doom/r_sky.c",
    "csrc/doom/r_things.c",
    "csrc/doom/s_sound.c",
    "csrc/doom/sha1.c",
    "csrc/doom/sounds.c",
    "csrc/doom/statdump.c",
    "csrc/doom/st_lib.c",
    "csrc/doom/st_stuff.c",
    "csrc/doom/tables.c",
    "csrc/doom/v_video.c",
    "csrc/doom/w_checksum.c",
    "csrc/doom/w_file.c",
    "csrc/doom/w_file_stdc.c",
    "csrc/doom/w_main.c",
    "csrc/doom/w_wad.c",
    "csrc/doom/wi_stuff.c",
    "csrc/doom/z_zone.c",
    "csrc/doom/doomgeneric_konjac.c",
];

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set by cargo"));
    let compiler = find_c_compiler();

    let mut object_paths = Vec::new();
    for src in C_SOURCES {
        println!("cargo:rerun-if-changed={src}");
        let src_path = Path::new(src);
        let obj_name = src_path.file_stem().unwrap().to_str().unwrap().to_owned() + ".o";
        let obj_path = out_dir.join(&obj_name);

        let status = Command::new(&compiler)
            .args([
                "-c",
                src,
                "-o",
                obj_path.to_str().unwrap(),
                "-ffreestanding",
                "-nostdinc",
                "-Icsrc/doom_include",
                "-fno-builtin",
                "-fno-stack-protector",
                "-fno-stack-check",
                "-fno-unwind-tables",
                "-fno-asynchronous-unwind-tables",
                "-fno-pic",
                "-fno-pie",
                "-mno-red-zone",
                // No -mno-sse/-mno-mmx here (unlike the flags this had
                // for the original cdemo.c-only groundwork): the DOOM
                // source uses float/double (transcendental math in
                // r_main.c's startup tables, mouse-accel in v_video.c),
                // and x86-64 SysV passes those in XMM registers by
                // default -- disabling SSE would silently switch to a
                // non-standard x87-based calling convention instead.
                // That's safe under KonjacOS's per-task FXSAVE/FXRSTOR
                // (covers x87 and SSE state alike), so there's no reason
                // to avoid it.
                "-mcmodel=kernel",
                "-m64",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Wno-unused-parameter",
                "-Wno-unused-variable",
                "-Wno-unused-function",
                "-Wno-sign-compare",
            ])
            .status()
            .unwrap_or_else(|e| panic!("failed to run {compiler:?} on {src}: {e}"));
        assert!(status.success(), "{compiler:?} failed to compile {src}");
        object_paths.push(obj_path);
    }

    let lib_path = out_dir.join("libcdoom.a");
    let mut ar_cmd = Command::new("ar");
    ar_cmd.arg("crs").arg(&lib_path);
    for obj in &object_paths {
        ar_cmd.arg(obj);
    }
    let status = ar_cmd.status().unwrap_or_else(|e| panic!("failed to run ar: {e}"));
    assert!(status.success(), "ar failed to archive {C_SOURCES:?} into {lib_path:?}");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=cdoom");
}

/// Prefers `clang` (this environment has it, and it accepts the exact same
/// flags as `gcc` for everything used here) but falls back to `cc`/`gcc`
/// so this still works in an environment where only one of the two is
/// installed.
fn find_c_compiler() -> String {
    for candidate in ["clang", "cc", "gcc"] {
        if Command::new(candidate).arg("--version").output().is_ok_and(|o| o.status.success()) {
            return candidate.to_owned();
        }
    }
    panic!("no C compiler found (looked for clang, cc, gcc) -- required for csrc/ (DOOM-porting groundwork)");
}
