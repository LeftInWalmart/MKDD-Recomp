//! Unit test for the paired-single math path: runs the recompiled SDK matrix/vector functions
//! on known inputs and compares with plain Rust. Does not boot the game.
//!
//!   cargo run -p mkdd-recomp --bin pstest

use gc_runtime::Ctx;

const A: u32 = 0x8100_0000;
const B: u32 = 0x8100_0100;
const OUT: u32 = 0x8100_0200;

fn put_mtx(c: &Ctx, addr: u32, m: &[[f32; 4]; 3]) {
    let mm = gc_runtime::machine();
    for (r, row) in m.iter().enumerate() {
        for (k, v) in row.iter().enumerate() {
            mm.write_be(addr + 4 * (4 * r as u32 + k as u32), 4, v.to_bits() as u64);
        }
    }
    let _ = c;
}

fn get_f32s(addr: u32, n: usize) -> Vec<f32> {
    let mm = gc_runtime::machine();
    (0..n).map(|i| f32::from_bits(mm.read_be(addr + 4 * i as u32, 4) as u32)).collect()
}

fn concat(a: &[[f32; 4]; 3], b: &[[f32; 4]; 3]) -> Vec<f32> {
    let mut out = vec![0f32; 12];
    for r in 0..3 {
        for k in 0..4 {
            let mut v = a[r][0] * b[0][k] + a[r][1] * b[1][k] + a[r][2] * b[2][k];
            if k == 3 {
                v += a[r][3];
            }
            out[4 * r + k] = v;
        }
    }
    out
}

fn report(name: &str, got: &[f32], want: &[f32]) -> bool {
    let ok = got.iter().zip(want).all(|(g, w)| (g - w).abs() <= 1e-4 * w.abs().max(1.0));
    println!("{} {name}", if ok { "[ok]  " } else { "[FAIL]" });
    if !ok {
        println!("   got  {got:?}\n   want {want:?}");
    }
    ok
}

fn call(c: &mut Ctx, name: &str, args: &[u32]) {
    let addr = mkdd_recomp::function_addr(name).unwrap_or_else(|| panic!("{name} not found"));
    c.call_guest(addr, args);
}

fn main() {
    let mut c = mkdd_recomp::boot();
    // MSR: FP available; GQRs zero (= plain f32 for psq_l/psq_st), as after OSInit.
    let a = [[1.0, 2.0, 3.0, 4.0], [5.0, 6.0, 7.0, 8.0], [9.0, 10.0, 11.0, 12.0]];
    let b = [[0.5, -1.0, 2.0, 3.0], [1.5, 0.25, -2.0, 1.0], [-0.5, 4.0, 1.0, -6.0]];
    put_mtx(&c, A, &a);
    put_mtx(&c, B, &b);
    let mut all = true;

    call(&mut c, "PSMTXCopy", &[A, OUT]);
    all &= report("PSMTXCopy", &get_f32s(OUT, 12), &a.concat());

    call(&mut c, "PSMTXConcat", &[A, B, OUT]);
    all &= report("PSMTXConcat", &get_f32s(OUT, 12), &concat(&a, &b));

    // PSMTXMultVec(m, src, dst)
    let v = [2.0f32, -3.0, 0.5];
    let vb = 0x8100_0300;
    for (i, x) in v.iter().enumerate() {
        gc_runtime::machine().write_be(vb + 4 * i as u32, 4, x.to_bits() as u64);
    }
    call(&mut c, "PSMTXMultVec", &[A, vb, OUT]);
    let want: Vec<f32> = (0..3).map(|r| a[r][0] * v[0] + a[r][1] * v[1] + a[r][2] * v[2] + a[r][3]).collect();
    all &= report("PSMTXMultVec", &get_f32s(OUT, 3), &want);

    // PSVECCrossProduct(a, b, out)
    let u = [1.0f32, 2.0, 3.0];
    let w = [-4.0f32, 0.5, 2.0];
    let ub = 0x8100_0400;
    let wb = 0x8100_0410;
    for i in 0..3 {
        gc_runtime::machine().write_be(ub + 4 * i as u32, 4, u[i].to_bits() as u64);
        gc_runtime::machine().write_be(wb + 4 * i as u32, 4, w[i].to_bits() as u64);
    }
    call(&mut c, "PSVECCrossProduct", &[ub, wb, OUT]);
    let want = [u[1] * w[2] - u[2] * w[1], u[2] * w[0] - u[0] * w[2], u[0] * w[1] - u[1] * w[0]];
    all &= report("PSVECCrossProduct", &get_f32s(OUT, 3), &want);

    println!("{}", if all { "ALL PASSED" } else { "FAILURES" });
    std::process::exit(if all { 0 } else { 1 });
}
