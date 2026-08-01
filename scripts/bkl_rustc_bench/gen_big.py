#!/usr/bin/env python3
"""Regenerate big.rs deterministically (codegen-bound contrast to hello.rs)."""
import sys
lines = ["// generated, deterministic; codegen-bound contrast to hello.rs",
         "#![allow(dead_code)]", "use std::collections::HashMap;"]
for i in range(400):
    lines.append(f"""
#[derive(Debug, Clone, PartialEq)]
pub struct S{i} {{ pub a: u64, pub b: String, pub c: Vec<u32> }}
impl S{i} {{
    pub fn new(a: u64) -> Self {{ Self {{ a, b: format!("s{i}-{{}}", a), c: (0..(a % 8) as u32).collect() }} }}
    pub fn score(&self) -> u64 {{ self.a.wrapping_mul({i + 1}) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }}
    pub fn index(&self) -> HashMap<String, u64> {{
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }}
}}""")
lines.append("\nfn main() {\n    let mut t = 0u64;")
for i in range(400):
    lines.append(f"    t = t.wrapping_add(S{i}::new({i} as u64).score()); let _ = S{i}::new(3).index();")
lines.append('    println!("{}", t);\n}')
open(sys.argv[1] if len(sys.argv) > 1 else "big.rs", "w").write("\n".join(lines))
