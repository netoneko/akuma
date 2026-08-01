// generated, deterministic; codegen-bound contrast to hello.rs
#![allow(dead_code)]
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct S0 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S0 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s0-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(1) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S1 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S1 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s1-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(2) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S2 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S2 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s2-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(3) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S3 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S3 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s3-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(4) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S4 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S4 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s4-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(5) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S5 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S5 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s5-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(6) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S6 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S6 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s6-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(7) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S7 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S7 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s7-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(8) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S8 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S8 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s8-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(9) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S9 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S9 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s9-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(10) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S10 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S10 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s10-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(11) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S11 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S11 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s11-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(12) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S12 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S12 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s12-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(13) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S13 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S13 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s13-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(14) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S14 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S14 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s14-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(15) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S15 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S15 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s15-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(16) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S16 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S16 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s16-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(17) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S17 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S17 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s17-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(18) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S18 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S18 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s18-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(19) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S19 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S19 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s19-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(20) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S20 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S20 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s20-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(21) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S21 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S21 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s21-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(22) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S22 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S22 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s22-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(23) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S23 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S23 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s23-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(24) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S24 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S24 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s24-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(25) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S25 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S25 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s25-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(26) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S26 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S26 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s26-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(27) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S27 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S27 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s27-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(28) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S28 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S28 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s28-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(29) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S29 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S29 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s29-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(30) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S30 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S30 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s30-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(31) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S31 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S31 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s31-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(32) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S32 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S32 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s32-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(33) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S33 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S33 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s33-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(34) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S34 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S34 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s34-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(35) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S35 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S35 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s35-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(36) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S36 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S36 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s36-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(37) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S37 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S37 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s37-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(38) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S38 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S38 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s38-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(39) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S39 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S39 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s39-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(40) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S40 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S40 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s40-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(41) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S41 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S41 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s41-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(42) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S42 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S42 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s42-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(43) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S43 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S43 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s43-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(44) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S44 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S44 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s44-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(45) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S45 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S45 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s45-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(46) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S46 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S46 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s46-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(47) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S47 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S47 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s47-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(48) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S48 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S48 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s48-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(49) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S49 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S49 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s49-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(50) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S50 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S50 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s50-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(51) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S51 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S51 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s51-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(52) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S52 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S52 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s52-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(53) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S53 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S53 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s53-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(54) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S54 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S54 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s54-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(55) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S55 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S55 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s55-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(56) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S56 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S56 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s56-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(57) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S57 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S57 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s57-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(58) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S58 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S58 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s58-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(59) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S59 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S59 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s59-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(60) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S60 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S60 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s60-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(61) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S61 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S61 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s61-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(62) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S62 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S62 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s62-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(63) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S63 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S63 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s63-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(64) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S64 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S64 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s64-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(65) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S65 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S65 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s65-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(66) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S66 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S66 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s66-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(67) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S67 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S67 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s67-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(68) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S68 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S68 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s68-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(69) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S69 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S69 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s69-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(70) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S70 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S70 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s70-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(71) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S71 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S71 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s71-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(72) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S72 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S72 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s72-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(73) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S73 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S73 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s73-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(74) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S74 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S74 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s74-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(75) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S75 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S75 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s75-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(76) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S76 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S76 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s76-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(77) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S77 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S77 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s77-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(78) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S78 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S78 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s78-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(79) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S79 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S79 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s79-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(80) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S80 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S80 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s80-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(81) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S81 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S81 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s81-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(82) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S82 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S82 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s82-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(83) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S83 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S83 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s83-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(84) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S84 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S84 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s84-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(85) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S85 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S85 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s85-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(86) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S86 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S86 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s86-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(87) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S87 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S87 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s87-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(88) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S88 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S88 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s88-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(89) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S89 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S89 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s89-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(90) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S90 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S90 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s90-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(91) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S91 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S91 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s91-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(92) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S92 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S92 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s92-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(93) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S93 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S93 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s93-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(94) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S94 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S94 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s94-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(95) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S95 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S95 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s95-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(96) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S96 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S96 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s96-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(97) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S97 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S97 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s97-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(98) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S98 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S98 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s98-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(99) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S99 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S99 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s99-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(100) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S100 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S100 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s100-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(101) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S101 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S101 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s101-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(102) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S102 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S102 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s102-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(103) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S103 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S103 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s103-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(104) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S104 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S104 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s104-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(105) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S105 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S105 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s105-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(106) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S106 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S106 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s106-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(107) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S107 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S107 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s107-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(108) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S108 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S108 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s108-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(109) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S109 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S109 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s109-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(110) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S110 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S110 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s110-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(111) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S111 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S111 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s111-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(112) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S112 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S112 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s112-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(113) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S113 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S113 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s113-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(114) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S114 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S114 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s114-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(115) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S115 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S115 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s115-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(116) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S116 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S116 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s116-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(117) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S117 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S117 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s117-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(118) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S118 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S118 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s118-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(119) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S119 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S119 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s119-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(120) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S120 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S120 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s120-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(121) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S121 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S121 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s121-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(122) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S122 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S122 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s122-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(123) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S123 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S123 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s123-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(124) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S124 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S124 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s124-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(125) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S125 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S125 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s125-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(126) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S126 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S126 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s126-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(127) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S127 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S127 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s127-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(128) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S128 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S128 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s128-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(129) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S129 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S129 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s129-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(130) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S130 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S130 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s130-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(131) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S131 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S131 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s131-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(132) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S132 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S132 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s132-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(133) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S133 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S133 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s133-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(134) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S134 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S134 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s134-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(135) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S135 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S135 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s135-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(136) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S136 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S136 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s136-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(137) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S137 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S137 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s137-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(138) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S138 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S138 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s138-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(139) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S139 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S139 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s139-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(140) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S140 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S140 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s140-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(141) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S141 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S141 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s141-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(142) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S142 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S142 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s142-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(143) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S143 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S143 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s143-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(144) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S144 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S144 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s144-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(145) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S145 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S145 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s145-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(146) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S146 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S146 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s146-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(147) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S147 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S147 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s147-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(148) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S148 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S148 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s148-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(149) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S149 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S149 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s149-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(150) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S150 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S150 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s150-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(151) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S151 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S151 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s151-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(152) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S152 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S152 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s152-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(153) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S153 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S153 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s153-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(154) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S154 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S154 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s154-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(155) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S155 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S155 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s155-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(156) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S156 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S156 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s156-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(157) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S157 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S157 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s157-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(158) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S158 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S158 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s158-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(159) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S159 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S159 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s159-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(160) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S160 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S160 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s160-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(161) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S161 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S161 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s161-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(162) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S162 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S162 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s162-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(163) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S163 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S163 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s163-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(164) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S164 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S164 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s164-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(165) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S165 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S165 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s165-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(166) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S166 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S166 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s166-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(167) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S167 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S167 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s167-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(168) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S168 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S168 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s168-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(169) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S169 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S169 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s169-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(170) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S170 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S170 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s170-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(171) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S171 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S171 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s171-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(172) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S172 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S172 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s172-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(173) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S173 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S173 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s173-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(174) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S174 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S174 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s174-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(175) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S175 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S175 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s175-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(176) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S176 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S176 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s176-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(177) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S177 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S177 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s177-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(178) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S178 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S178 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s178-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(179) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S179 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S179 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s179-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(180) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S180 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S180 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s180-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(181) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S181 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S181 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s181-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(182) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S182 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S182 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s182-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(183) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S183 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S183 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s183-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(184) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S184 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S184 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s184-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(185) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S185 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S185 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s185-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(186) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S186 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S186 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s186-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(187) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S187 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S187 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s187-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(188) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S188 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S188 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s188-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(189) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S189 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S189 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s189-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(190) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S190 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S190 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s190-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(191) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S191 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S191 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s191-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(192) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S192 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S192 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s192-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(193) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S193 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S193 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s193-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(194) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S194 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S194 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s194-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(195) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S195 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S195 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s195-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(196) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S196 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S196 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s196-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(197) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S197 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S197 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s197-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(198) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S198 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S198 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s198-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(199) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S199 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S199 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s199-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(200) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S200 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S200 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s200-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(201) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S201 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S201 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s201-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(202) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S202 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S202 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s202-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(203) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S203 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S203 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s203-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(204) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S204 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S204 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s204-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(205) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S205 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S205 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s205-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(206) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S206 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S206 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s206-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(207) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S207 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S207 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s207-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(208) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S208 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S208 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s208-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(209) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S209 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S209 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s209-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(210) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S210 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S210 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s210-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(211) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S211 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S211 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s211-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(212) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S212 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S212 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s212-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(213) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S213 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S213 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s213-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(214) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S214 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S214 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s214-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(215) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S215 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S215 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s215-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(216) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S216 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S216 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s216-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(217) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S217 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S217 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s217-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(218) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S218 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S218 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s218-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(219) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S219 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S219 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s219-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(220) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S220 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S220 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s220-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(221) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S221 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S221 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s221-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(222) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S222 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S222 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s222-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(223) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S223 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S223 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s223-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(224) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S224 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S224 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s224-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(225) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S225 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S225 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s225-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(226) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S226 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S226 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s226-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(227) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S227 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S227 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s227-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(228) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S228 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S228 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s228-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(229) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S229 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S229 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s229-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(230) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S230 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S230 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s230-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(231) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S231 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S231 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s231-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(232) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S232 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S232 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s232-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(233) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S233 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S233 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s233-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(234) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S234 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S234 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s234-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(235) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S235 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S235 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s235-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(236) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S236 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S236 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s236-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(237) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S237 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S237 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s237-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(238) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S238 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S238 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s238-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(239) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S239 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S239 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s239-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(240) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S240 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S240 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s240-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(241) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S241 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S241 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s241-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(242) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S242 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S242 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s242-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(243) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S243 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S243 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s243-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(244) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S244 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S244 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s244-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(245) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S245 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S245 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s245-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(246) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S246 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S246 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s246-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(247) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S247 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S247 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s247-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(248) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S248 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S248 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s248-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(249) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S249 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S249 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s249-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(250) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S250 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S250 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s250-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(251) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S251 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S251 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s251-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(252) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S252 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S252 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s252-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(253) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S253 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S253 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s253-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(254) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S254 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S254 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s254-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(255) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S255 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S255 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s255-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(256) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S256 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S256 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s256-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(257) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S257 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S257 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s257-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(258) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S258 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S258 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s258-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(259) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S259 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S259 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s259-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(260) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S260 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S260 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s260-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(261) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S261 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S261 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s261-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(262) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S262 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S262 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s262-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(263) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S263 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S263 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s263-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(264) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S264 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S264 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s264-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(265) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S265 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S265 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s265-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(266) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S266 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S266 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s266-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(267) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S267 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S267 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s267-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(268) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S268 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S268 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s268-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(269) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S269 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S269 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s269-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(270) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S270 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S270 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s270-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(271) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S271 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S271 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s271-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(272) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S272 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S272 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s272-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(273) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S273 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S273 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s273-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(274) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S274 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S274 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s274-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(275) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S275 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S275 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s275-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(276) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S276 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S276 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s276-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(277) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S277 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S277 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s277-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(278) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S278 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S278 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s278-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(279) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S279 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S279 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s279-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(280) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S280 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S280 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s280-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(281) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S281 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S281 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s281-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(282) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S282 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S282 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s282-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(283) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S283 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S283 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s283-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(284) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S284 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S284 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s284-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(285) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S285 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S285 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s285-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(286) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S286 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S286 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s286-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(287) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S287 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S287 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s287-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(288) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S288 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S288 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s288-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(289) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S289 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S289 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s289-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(290) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S290 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S290 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s290-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(291) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S291 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S291 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s291-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(292) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S292 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S292 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s292-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(293) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S293 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S293 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s293-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(294) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S294 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S294 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s294-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(295) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S295 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S295 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s295-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(296) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S296 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S296 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s296-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(297) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S297 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S297 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s297-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(298) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S298 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S298 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s298-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(299) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S299 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S299 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s299-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(300) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S300 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S300 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s300-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(301) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S301 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S301 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s301-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(302) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S302 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S302 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s302-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(303) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S303 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S303 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s303-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(304) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S304 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S304 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s304-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(305) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S305 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S305 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s305-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(306) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S306 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S306 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s306-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(307) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S307 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S307 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s307-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(308) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S308 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S308 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s308-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(309) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S309 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S309 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s309-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(310) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S310 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S310 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s310-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(311) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S311 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S311 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s311-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(312) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S312 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S312 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s312-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(313) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S313 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S313 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s313-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(314) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S314 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S314 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s314-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(315) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S315 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S315 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s315-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(316) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S316 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S316 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s316-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(317) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S317 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S317 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s317-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(318) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S318 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S318 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s318-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(319) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S319 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S319 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s319-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(320) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S320 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S320 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s320-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(321) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S321 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S321 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s321-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(322) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S322 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S322 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s322-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(323) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S323 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S323 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s323-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(324) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S324 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S324 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s324-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(325) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S325 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S325 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s325-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(326) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S326 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S326 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s326-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(327) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S327 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S327 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s327-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(328) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S328 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S328 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s328-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(329) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S329 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S329 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s329-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(330) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S330 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S330 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s330-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(331) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S331 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S331 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s331-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(332) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S332 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S332 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s332-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(333) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S333 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S333 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s333-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(334) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S334 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S334 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s334-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(335) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S335 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S335 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s335-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(336) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S336 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S336 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s336-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(337) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S337 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S337 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s337-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(338) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S338 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S338 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s338-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(339) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S339 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S339 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s339-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(340) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S340 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S340 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s340-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(341) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S341 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S341 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s341-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(342) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S342 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S342 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s342-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(343) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S343 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S343 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s343-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(344) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S344 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S344 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s344-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(345) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S345 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S345 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s345-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(346) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S346 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S346 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s346-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(347) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S347 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S347 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s347-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(348) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S348 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S348 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s348-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(349) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S349 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S349 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s349-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(350) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S350 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S350 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s350-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(351) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S351 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S351 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s351-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(352) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S352 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S352 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s352-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(353) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S353 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S353 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s353-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(354) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S354 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S354 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s354-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(355) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S355 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S355 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s355-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(356) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S356 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S356 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s356-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(357) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S357 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S357 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s357-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(358) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S358 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S358 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s358-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(359) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S359 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S359 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s359-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(360) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S360 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S360 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s360-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(361) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S361 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S361 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s361-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(362) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S362 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S362 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s362-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(363) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S363 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S363 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s363-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(364) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S364 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S364 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s364-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(365) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S365 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S365 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s365-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(366) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S366 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S366 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s366-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(367) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S367 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S367 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s367-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(368) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S368 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S368 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s368-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(369) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S369 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S369 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s369-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(370) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S370 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S370 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s370-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(371) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S371 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S371 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s371-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(372) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S372 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S372 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s372-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(373) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S373 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S373 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s373-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(374) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S374 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S374 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s374-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(375) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S375 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S375 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s375-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(376) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S376 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S376 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s376-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(377) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S377 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S377 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s377-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(378) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S378 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S378 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s378-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(379) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S379 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S379 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s379-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(380) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S380 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S380 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s380-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(381) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S381 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S381 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s381-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(382) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S382 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S382 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s382-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(383) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S383 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S383 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s383-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(384) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S384 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S384 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s384-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(385) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S385 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S385 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s385-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(386) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S386 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S386 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s386-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(387) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S387 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S387 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s387-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(388) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S388 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S388 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s388-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(389) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S389 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S389 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s389-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(390) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S390 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S390 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s390-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(391) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S391 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S391 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s391-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(392) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S392 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S392 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s392-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(393) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S393 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S393 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s393-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(394) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S394 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S394 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s394-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(395) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S395 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S395 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s395-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(396) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S396 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S396 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s396-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(397) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S397 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S397 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s397-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(398) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S398 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S398 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s398-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(399) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S399 { pub a: u64, pub b: String, pub c: Vec<u32> }
impl S399 {
    pub fn new(a: u64) -> Self { Self { a, b: format!("s399-{}", a), c: (0..(a % 8) as u32).collect() } }
    pub fn score(&self) -> u64 { self.a.wrapping_mul(400) ^ self.c.iter().map(|x| *x as u64).sum::<u64>() }
    pub fn index(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new(); m.insert(self.b.clone(), self.score()); m
    }
}

fn main() {
    let mut t = 0u64;
    t = t.wrapping_add(S0::new(0 as u64).score()); let _ = S0::new(3).index();
    t = t.wrapping_add(S1::new(1 as u64).score()); let _ = S1::new(3).index();
    t = t.wrapping_add(S2::new(2 as u64).score()); let _ = S2::new(3).index();
    t = t.wrapping_add(S3::new(3 as u64).score()); let _ = S3::new(3).index();
    t = t.wrapping_add(S4::new(4 as u64).score()); let _ = S4::new(3).index();
    t = t.wrapping_add(S5::new(5 as u64).score()); let _ = S5::new(3).index();
    t = t.wrapping_add(S6::new(6 as u64).score()); let _ = S6::new(3).index();
    t = t.wrapping_add(S7::new(7 as u64).score()); let _ = S7::new(3).index();
    t = t.wrapping_add(S8::new(8 as u64).score()); let _ = S8::new(3).index();
    t = t.wrapping_add(S9::new(9 as u64).score()); let _ = S9::new(3).index();
    t = t.wrapping_add(S10::new(10 as u64).score()); let _ = S10::new(3).index();
    t = t.wrapping_add(S11::new(11 as u64).score()); let _ = S11::new(3).index();
    t = t.wrapping_add(S12::new(12 as u64).score()); let _ = S12::new(3).index();
    t = t.wrapping_add(S13::new(13 as u64).score()); let _ = S13::new(3).index();
    t = t.wrapping_add(S14::new(14 as u64).score()); let _ = S14::new(3).index();
    t = t.wrapping_add(S15::new(15 as u64).score()); let _ = S15::new(3).index();
    t = t.wrapping_add(S16::new(16 as u64).score()); let _ = S16::new(3).index();
    t = t.wrapping_add(S17::new(17 as u64).score()); let _ = S17::new(3).index();
    t = t.wrapping_add(S18::new(18 as u64).score()); let _ = S18::new(3).index();
    t = t.wrapping_add(S19::new(19 as u64).score()); let _ = S19::new(3).index();
    t = t.wrapping_add(S20::new(20 as u64).score()); let _ = S20::new(3).index();
    t = t.wrapping_add(S21::new(21 as u64).score()); let _ = S21::new(3).index();
    t = t.wrapping_add(S22::new(22 as u64).score()); let _ = S22::new(3).index();
    t = t.wrapping_add(S23::new(23 as u64).score()); let _ = S23::new(3).index();
    t = t.wrapping_add(S24::new(24 as u64).score()); let _ = S24::new(3).index();
    t = t.wrapping_add(S25::new(25 as u64).score()); let _ = S25::new(3).index();
    t = t.wrapping_add(S26::new(26 as u64).score()); let _ = S26::new(3).index();
    t = t.wrapping_add(S27::new(27 as u64).score()); let _ = S27::new(3).index();
    t = t.wrapping_add(S28::new(28 as u64).score()); let _ = S28::new(3).index();
    t = t.wrapping_add(S29::new(29 as u64).score()); let _ = S29::new(3).index();
    t = t.wrapping_add(S30::new(30 as u64).score()); let _ = S30::new(3).index();
    t = t.wrapping_add(S31::new(31 as u64).score()); let _ = S31::new(3).index();
    t = t.wrapping_add(S32::new(32 as u64).score()); let _ = S32::new(3).index();
    t = t.wrapping_add(S33::new(33 as u64).score()); let _ = S33::new(3).index();
    t = t.wrapping_add(S34::new(34 as u64).score()); let _ = S34::new(3).index();
    t = t.wrapping_add(S35::new(35 as u64).score()); let _ = S35::new(3).index();
    t = t.wrapping_add(S36::new(36 as u64).score()); let _ = S36::new(3).index();
    t = t.wrapping_add(S37::new(37 as u64).score()); let _ = S37::new(3).index();
    t = t.wrapping_add(S38::new(38 as u64).score()); let _ = S38::new(3).index();
    t = t.wrapping_add(S39::new(39 as u64).score()); let _ = S39::new(3).index();
    t = t.wrapping_add(S40::new(40 as u64).score()); let _ = S40::new(3).index();
    t = t.wrapping_add(S41::new(41 as u64).score()); let _ = S41::new(3).index();
    t = t.wrapping_add(S42::new(42 as u64).score()); let _ = S42::new(3).index();
    t = t.wrapping_add(S43::new(43 as u64).score()); let _ = S43::new(3).index();
    t = t.wrapping_add(S44::new(44 as u64).score()); let _ = S44::new(3).index();
    t = t.wrapping_add(S45::new(45 as u64).score()); let _ = S45::new(3).index();
    t = t.wrapping_add(S46::new(46 as u64).score()); let _ = S46::new(3).index();
    t = t.wrapping_add(S47::new(47 as u64).score()); let _ = S47::new(3).index();
    t = t.wrapping_add(S48::new(48 as u64).score()); let _ = S48::new(3).index();
    t = t.wrapping_add(S49::new(49 as u64).score()); let _ = S49::new(3).index();
    t = t.wrapping_add(S50::new(50 as u64).score()); let _ = S50::new(3).index();
    t = t.wrapping_add(S51::new(51 as u64).score()); let _ = S51::new(3).index();
    t = t.wrapping_add(S52::new(52 as u64).score()); let _ = S52::new(3).index();
    t = t.wrapping_add(S53::new(53 as u64).score()); let _ = S53::new(3).index();
    t = t.wrapping_add(S54::new(54 as u64).score()); let _ = S54::new(3).index();
    t = t.wrapping_add(S55::new(55 as u64).score()); let _ = S55::new(3).index();
    t = t.wrapping_add(S56::new(56 as u64).score()); let _ = S56::new(3).index();
    t = t.wrapping_add(S57::new(57 as u64).score()); let _ = S57::new(3).index();
    t = t.wrapping_add(S58::new(58 as u64).score()); let _ = S58::new(3).index();
    t = t.wrapping_add(S59::new(59 as u64).score()); let _ = S59::new(3).index();
    t = t.wrapping_add(S60::new(60 as u64).score()); let _ = S60::new(3).index();
    t = t.wrapping_add(S61::new(61 as u64).score()); let _ = S61::new(3).index();
    t = t.wrapping_add(S62::new(62 as u64).score()); let _ = S62::new(3).index();
    t = t.wrapping_add(S63::new(63 as u64).score()); let _ = S63::new(3).index();
    t = t.wrapping_add(S64::new(64 as u64).score()); let _ = S64::new(3).index();
    t = t.wrapping_add(S65::new(65 as u64).score()); let _ = S65::new(3).index();
    t = t.wrapping_add(S66::new(66 as u64).score()); let _ = S66::new(3).index();
    t = t.wrapping_add(S67::new(67 as u64).score()); let _ = S67::new(3).index();
    t = t.wrapping_add(S68::new(68 as u64).score()); let _ = S68::new(3).index();
    t = t.wrapping_add(S69::new(69 as u64).score()); let _ = S69::new(3).index();
    t = t.wrapping_add(S70::new(70 as u64).score()); let _ = S70::new(3).index();
    t = t.wrapping_add(S71::new(71 as u64).score()); let _ = S71::new(3).index();
    t = t.wrapping_add(S72::new(72 as u64).score()); let _ = S72::new(3).index();
    t = t.wrapping_add(S73::new(73 as u64).score()); let _ = S73::new(3).index();
    t = t.wrapping_add(S74::new(74 as u64).score()); let _ = S74::new(3).index();
    t = t.wrapping_add(S75::new(75 as u64).score()); let _ = S75::new(3).index();
    t = t.wrapping_add(S76::new(76 as u64).score()); let _ = S76::new(3).index();
    t = t.wrapping_add(S77::new(77 as u64).score()); let _ = S77::new(3).index();
    t = t.wrapping_add(S78::new(78 as u64).score()); let _ = S78::new(3).index();
    t = t.wrapping_add(S79::new(79 as u64).score()); let _ = S79::new(3).index();
    t = t.wrapping_add(S80::new(80 as u64).score()); let _ = S80::new(3).index();
    t = t.wrapping_add(S81::new(81 as u64).score()); let _ = S81::new(3).index();
    t = t.wrapping_add(S82::new(82 as u64).score()); let _ = S82::new(3).index();
    t = t.wrapping_add(S83::new(83 as u64).score()); let _ = S83::new(3).index();
    t = t.wrapping_add(S84::new(84 as u64).score()); let _ = S84::new(3).index();
    t = t.wrapping_add(S85::new(85 as u64).score()); let _ = S85::new(3).index();
    t = t.wrapping_add(S86::new(86 as u64).score()); let _ = S86::new(3).index();
    t = t.wrapping_add(S87::new(87 as u64).score()); let _ = S87::new(3).index();
    t = t.wrapping_add(S88::new(88 as u64).score()); let _ = S88::new(3).index();
    t = t.wrapping_add(S89::new(89 as u64).score()); let _ = S89::new(3).index();
    t = t.wrapping_add(S90::new(90 as u64).score()); let _ = S90::new(3).index();
    t = t.wrapping_add(S91::new(91 as u64).score()); let _ = S91::new(3).index();
    t = t.wrapping_add(S92::new(92 as u64).score()); let _ = S92::new(3).index();
    t = t.wrapping_add(S93::new(93 as u64).score()); let _ = S93::new(3).index();
    t = t.wrapping_add(S94::new(94 as u64).score()); let _ = S94::new(3).index();
    t = t.wrapping_add(S95::new(95 as u64).score()); let _ = S95::new(3).index();
    t = t.wrapping_add(S96::new(96 as u64).score()); let _ = S96::new(3).index();
    t = t.wrapping_add(S97::new(97 as u64).score()); let _ = S97::new(3).index();
    t = t.wrapping_add(S98::new(98 as u64).score()); let _ = S98::new(3).index();
    t = t.wrapping_add(S99::new(99 as u64).score()); let _ = S99::new(3).index();
    t = t.wrapping_add(S100::new(100 as u64).score()); let _ = S100::new(3).index();
    t = t.wrapping_add(S101::new(101 as u64).score()); let _ = S101::new(3).index();
    t = t.wrapping_add(S102::new(102 as u64).score()); let _ = S102::new(3).index();
    t = t.wrapping_add(S103::new(103 as u64).score()); let _ = S103::new(3).index();
    t = t.wrapping_add(S104::new(104 as u64).score()); let _ = S104::new(3).index();
    t = t.wrapping_add(S105::new(105 as u64).score()); let _ = S105::new(3).index();
    t = t.wrapping_add(S106::new(106 as u64).score()); let _ = S106::new(3).index();
    t = t.wrapping_add(S107::new(107 as u64).score()); let _ = S107::new(3).index();
    t = t.wrapping_add(S108::new(108 as u64).score()); let _ = S108::new(3).index();
    t = t.wrapping_add(S109::new(109 as u64).score()); let _ = S109::new(3).index();
    t = t.wrapping_add(S110::new(110 as u64).score()); let _ = S110::new(3).index();
    t = t.wrapping_add(S111::new(111 as u64).score()); let _ = S111::new(3).index();
    t = t.wrapping_add(S112::new(112 as u64).score()); let _ = S112::new(3).index();
    t = t.wrapping_add(S113::new(113 as u64).score()); let _ = S113::new(3).index();
    t = t.wrapping_add(S114::new(114 as u64).score()); let _ = S114::new(3).index();
    t = t.wrapping_add(S115::new(115 as u64).score()); let _ = S115::new(3).index();
    t = t.wrapping_add(S116::new(116 as u64).score()); let _ = S116::new(3).index();
    t = t.wrapping_add(S117::new(117 as u64).score()); let _ = S117::new(3).index();
    t = t.wrapping_add(S118::new(118 as u64).score()); let _ = S118::new(3).index();
    t = t.wrapping_add(S119::new(119 as u64).score()); let _ = S119::new(3).index();
    t = t.wrapping_add(S120::new(120 as u64).score()); let _ = S120::new(3).index();
    t = t.wrapping_add(S121::new(121 as u64).score()); let _ = S121::new(3).index();
    t = t.wrapping_add(S122::new(122 as u64).score()); let _ = S122::new(3).index();
    t = t.wrapping_add(S123::new(123 as u64).score()); let _ = S123::new(3).index();
    t = t.wrapping_add(S124::new(124 as u64).score()); let _ = S124::new(3).index();
    t = t.wrapping_add(S125::new(125 as u64).score()); let _ = S125::new(3).index();
    t = t.wrapping_add(S126::new(126 as u64).score()); let _ = S126::new(3).index();
    t = t.wrapping_add(S127::new(127 as u64).score()); let _ = S127::new(3).index();
    t = t.wrapping_add(S128::new(128 as u64).score()); let _ = S128::new(3).index();
    t = t.wrapping_add(S129::new(129 as u64).score()); let _ = S129::new(3).index();
    t = t.wrapping_add(S130::new(130 as u64).score()); let _ = S130::new(3).index();
    t = t.wrapping_add(S131::new(131 as u64).score()); let _ = S131::new(3).index();
    t = t.wrapping_add(S132::new(132 as u64).score()); let _ = S132::new(3).index();
    t = t.wrapping_add(S133::new(133 as u64).score()); let _ = S133::new(3).index();
    t = t.wrapping_add(S134::new(134 as u64).score()); let _ = S134::new(3).index();
    t = t.wrapping_add(S135::new(135 as u64).score()); let _ = S135::new(3).index();
    t = t.wrapping_add(S136::new(136 as u64).score()); let _ = S136::new(3).index();
    t = t.wrapping_add(S137::new(137 as u64).score()); let _ = S137::new(3).index();
    t = t.wrapping_add(S138::new(138 as u64).score()); let _ = S138::new(3).index();
    t = t.wrapping_add(S139::new(139 as u64).score()); let _ = S139::new(3).index();
    t = t.wrapping_add(S140::new(140 as u64).score()); let _ = S140::new(3).index();
    t = t.wrapping_add(S141::new(141 as u64).score()); let _ = S141::new(3).index();
    t = t.wrapping_add(S142::new(142 as u64).score()); let _ = S142::new(3).index();
    t = t.wrapping_add(S143::new(143 as u64).score()); let _ = S143::new(3).index();
    t = t.wrapping_add(S144::new(144 as u64).score()); let _ = S144::new(3).index();
    t = t.wrapping_add(S145::new(145 as u64).score()); let _ = S145::new(3).index();
    t = t.wrapping_add(S146::new(146 as u64).score()); let _ = S146::new(3).index();
    t = t.wrapping_add(S147::new(147 as u64).score()); let _ = S147::new(3).index();
    t = t.wrapping_add(S148::new(148 as u64).score()); let _ = S148::new(3).index();
    t = t.wrapping_add(S149::new(149 as u64).score()); let _ = S149::new(3).index();
    t = t.wrapping_add(S150::new(150 as u64).score()); let _ = S150::new(3).index();
    t = t.wrapping_add(S151::new(151 as u64).score()); let _ = S151::new(3).index();
    t = t.wrapping_add(S152::new(152 as u64).score()); let _ = S152::new(3).index();
    t = t.wrapping_add(S153::new(153 as u64).score()); let _ = S153::new(3).index();
    t = t.wrapping_add(S154::new(154 as u64).score()); let _ = S154::new(3).index();
    t = t.wrapping_add(S155::new(155 as u64).score()); let _ = S155::new(3).index();
    t = t.wrapping_add(S156::new(156 as u64).score()); let _ = S156::new(3).index();
    t = t.wrapping_add(S157::new(157 as u64).score()); let _ = S157::new(3).index();
    t = t.wrapping_add(S158::new(158 as u64).score()); let _ = S158::new(3).index();
    t = t.wrapping_add(S159::new(159 as u64).score()); let _ = S159::new(3).index();
    t = t.wrapping_add(S160::new(160 as u64).score()); let _ = S160::new(3).index();
    t = t.wrapping_add(S161::new(161 as u64).score()); let _ = S161::new(3).index();
    t = t.wrapping_add(S162::new(162 as u64).score()); let _ = S162::new(3).index();
    t = t.wrapping_add(S163::new(163 as u64).score()); let _ = S163::new(3).index();
    t = t.wrapping_add(S164::new(164 as u64).score()); let _ = S164::new(3).index();
    t = t.wrapping_add(S165::new(165 as u64).score()); let _ = S165::new(3).index();
    t = t.wrapping_add(S166::new(166 as u64).score()); let _ = S166::new(3).index();
    t = t.wrapping_add(S167::new(167 as u64).score()); let _ = S167::new(3).index();
    t = t.wrapping_add(S168::new(168 as u64).score()); let _ = S168::new(3).index();
    t = t.wrapping_add(S169::new(169 as u64).score()); let _ = S169::new(3).index();
    t = t.wrapping_add(S170::new(170 as u64).score()); let _ = S170::new(3).index();
    t = t.wrapping_add(S171::new(171 as u64).score()); let _ = S171::new(3).index();
    t = t.wrapping_add(S172::new(172 as u64).score()); let _ = S172::new(3).index();
    t = t.wrapping_add(S173::new(173 as u64).score()); let _ = S173::new(3).index();
    t = t.wrapping_add(S174::new(174 as u64).score()); let _ = S174::new(3).index();
    t = t.wrapping_add(S175::new(175 as u64).score()); let _ = S175::new(3).index();
    t = t.wrapping_add(S176::new(176 as u64).score()); let _ = S176::new(3).index();
    t = t.wrapping_add(S177::new(177 as u64).score()); let _ = S177::new(3).index();
    t = t.wrapping_add(S178::new(178 as u64).score()); let _ = S178::new(3).index();
    t = t.wrapping_add(S179::new(179 as u64).score()); let _ = S179::new(3).index();
    t = t.wrapping_add(S180::new(180 as u64).score()); let _ = S180::new(3).index();
    t = t.wrapping_add(S181::new(181 as u64).score()); let _ = S181::new(3).index();
    t = t.wrapping_add(S182::new(182 as u64).score()); let _ = S182::new(3).index();
    t = t.wrapping_add(S183::new(183 as u64).score()); let _ = S183::new(3).index();
    t = t.wrapping_add(S184::new(184 as u64).score()); let _ = S184::new(3).index();
    t = t.wrapping_add(S185::new(185 as u64).score()); let _ = S185::new(3).index();
    t = t.wrapping_add(S186::new(186 as u64).score()); let _ = S186::new(3).index();
    t = t.wrapping_add(S187::new(187 as u64).score()); let _ = S187::new(3).index();
    t = t.wrapping_add(S188::new(188 as u64).score()); let _ = S188::new(3).index();
    t = t.wrapping_add(S189::new(189 as u64).score()); let _ = S189::new(3).index();
    t = t.wrapping_add(S190::new(190 as u64).score()); let _ = S190::new(3).index();
    t = t.wrapping_add(S191::new(191 as u64).score()); let _ = S191::new(3).index();
    t = t.wrapping_add(S192::new(192 as u64).score()); let _ = S192::new(3).index();
    t = t.wrapping_add(S193::new(193 as u64).score()); let _ = S193::new(3).index();
    t = t.wrapping_add(S194::new(194 as u64).score()); let _ = S194::new(3).index();
    t = t.wrapping_add(S195::new(195 as u64).score()); let _ = S195::new(3).index();
    t = t.wrapping_add(S196::new(196 as u64).score()); let _ = S196::new(3).index();
    t = t.wrapping_add(S197::new(197 as u64).score()); let _ = S197::new(3).index();
    t = t.wrapping_add(S198::new(198 as u64).score()); let _ = S198::new(3).index();
    t = t.wrapping_add(S199::new(199 as u64).score()); let _ = S199::new(3).index();
    t = t.wrapping_add(S200::new(200 as u64).score()); let _ = S200::new(3).index();
    t = t.wrapping_add(S201::new(201 as u64).score()); let _ = S201::new(3).index();
    t = t.wrapping_add(S202::new(202 as u64).score()); let _ = S202::new(3).index();
    t = t.wrapping_add(S203::new(203 as u64).score()); let _ = S203::new(3).index();
    t = t.wrapping_add(S204::new(204 as u64).score()); let _ = S204::new(3).index();
    t = t.wrapping_add(S205::new(205 as u64).score()); let _ = S205::new(3).index();
    t = t.wrapping_add(S206::new(206 as u64).score()); let _ = S206::new(3).index();
    t = t.wrapping_add(S207::new(207 as u64).score()); let _ = S207::new(3).index();
    t = t.wrapping_add(S208::new(208 as u64).score()); let _ = S208::new(3).index();
    t = t.wrapping_add(S209::new(209 as u64).score()); let _ = S209::new(3).index();
    t = t.wrapping_add(S210::new(210 as u64).score()); let _ = S210::new(3).index();
    t = t.wrapping_add(S211::new(211 as u64).score()); let _ = S211::new(3).index();
    t = t.wrapping_add(S212::new(212 as u64).score()); let _ = S212::new(3).index();
    t = t.wrapping_add(S213::new(213 as u64).score()); let _ = S213::new(3).index();
    t = t.wrapping_add(S214::new(214 as u64).score()); let _ = S214::new(3).index();
    t = t.wrapping_add(S215::new(215 as u64).score()); let _ = S215::new(3).index();
    t = t.wrapping_add(S216::new(216 as u64).score()); let _ = S216::new(3).index();
    t = t.wrapping_add(S217::new(217 as u64).score()); let _ = S217::new(3).index();
    t = t.wrapping_add(S218::new(218 as u64).score()); let _ = S218::new(3).index();
    t = t.wrapping_add(S219::new(219 as u64).score()); let _ = S219::new(3).index();
    t = t.wrapping_add(S220::new(220 as u64).score()); let _ = S220::new(3).index();
    t = t.wrapping_add(S221::new(221 as u64).score()); let _ = S221::new(3).index();
    t = t.wrapping_add(S222::new(222 as u64).score()); let _ = S222::new(3).index();
    t = t.wrapping_add(S223::new(223 as u64).score()); let _ = S223::new(3).index();
    t = t.wrapping_add(S224::new(224 as u64).score()); let _ = S224::new(3).index();
    t = t.wrapping_add(S225::new(225 as u64).score()); let _ = S225::new(3).index();
    t = t.wrapping_add(S226::new(226 as u64).score()); let _ = S226::new(3).index();
    t = t.wrapping_add(S227::new(227 as u64).score()); let _ = S227::new(3).index();
    t = t.wrapping_add(S228::new(228 as u64).score()); let _ = S228::new(3).index();
    t = t.wrapping_add(S229::new(229 as u64).score()); let _ = S229::new(3).index();
    t = t.wrapping_add(S230::new(230 as u64).score()); let _ = S230::new(3).index();
    t = t.wrapping_add(S231::new(231 as u64).score()); let _ = S231::new(3).index();
    t = t.wrapping_add(S232::new(232 as u64).score()); let _ = S232::new(3).index();
    t = t.wrapping_add(S233::new(233 as u64).score()); let _ = S233::new(3).index();
    t = t.wrapping_add(S234::new(234 as u64).score()); let _ = S234::new(3).index();
    t = t.wrapping_add(S235::new(235 as u64).score()); let _ = S235::new(3).index();
    t = t.wrapping_add(S236::new(236 as u64).score()); let _ = S236::new(3).index();
    t = t.wrapping_add(S237::new(237 as u64).score()); let _ = S237::new(3).index();
    t = t.wrapping_add(S238::new(238 as u64).score()); let _ = S238::new(3).index();
    t = t.wrapping_add(S239::new(239 as u64).score()); let _ = S239::new(3).index();
    t = t.wrapping_add(S240::new(240 as u64).score()); let _ = S240::new(3).index();
    t = t.wrapping_add(S241::new(241 as u64).score()); let _ = S241::new(3).index();
    t = t.wrapping_add(S242::new(242 as u64).score()); let _ = S242::new(3).index();
    t = t.wrapping_add(S243::new(243 as u64).score()); let _ = S243::new(3).index();
    t = t.wrapping_add(S244::new(244 as u64).score()); let _ = S244::new(3).index();
    t = t.wrapping_add(S245::new(245 as u64).score()); let _ = S245::new(3).index();
    t = t.wrapping_add(S246::new(246 as u64).score()); let _ = S246::new(3).index();
    t = t.wrapping_add(S247::new(247 as u64).score()); let _ = S247::new(3).index();
    t = t.wrapping_add(S248::new(248 as u64).score()); let _ = S248::new(3).index();
    t = t.wrapping_add(S249::new(249 as u64).score()); let _ = S249::new(3).index();
    t = t.wrapping_add(S250::new(250 as u64).score()); let _ = S250::new(3).index();
    t = t.wrapping_add(S251::new(251 as u64).score()); let _ = S251::new(3).index();
    t = t.wrapping_add(S252::new(252 as u64).score()); let _ = S252::new(3).index();
    t = t.wrapping_add(S253::new(253 as u64).score()); let _ = S253::new(3).index();
    t = t.wrapping_add(S254::new(254 as u64).score()); let _ = S254::new(3).index();
    t = t.wrapping_add(S255::new(255 as u64).score()); let _ = S255::new(3).index();
    t = t.wrapping_add(S256::new(256 as u64).score()); let _ = S256::new(3).index();
    t = t.wrapping_add(S257::new(257 as u64).score()); let _ = S257::new(3).index();
    t = t.wrapping_add(S258::new(258 as u64).score()); let _ = S258::new(3).index();
    t = t.wrapping_add(S259::new(259 as u64).score()); let _ = S259::new(3).index();
    t = t.wrapping_add(S260::new(260 as u64).score()); let _ = S260::new(3).index();
    t = t.wrapping_add(S261::new(261 as u64).score()); let _ = S261::new(3).index();
    t = t.wrapping_add(S262::new(262 as u64).score()); let _ = S262::new(3).index();
    t = t.wrapping_add(S263::new(263 as u64).score()); let _ = S263::new(3).index();
    t = t.wrapping_add(S264::new(264 as u64).score()); let _ = S264::new(3).index();
    t = t.wrapping_add(S265::new(265 as u64).score()); let _ = S265::new(3).index();
    t = t.wrapping_add(S266::new(266 as u64).score()); let _ = S266::new(3).index();
    t = t.wrapping_add(S267::new(267 as u64).score()); let _ = S267::new(3).index();
    t = t.wrapping_add(S268::new(268 as u64).score()); let _ = S268::new(3).index();
    t = t.wrapping_add(S269::new(269 as u64).score()); let _ = S269::new(3).index();
    t = t.wrapping_add(S270::new(270 as u64).score()); let _ = S270::new(3).index();
    t = t.wrapping_add(S271::new(271 as u64).score()); let _ = S271::new(3).index();
    t = t.wrapping_add(S272::new(272 as u64).score()); let _ = S272::new(3).index();
    t = t.wrapping_add(S273::new(273 as u64).score()); let _ = S273::new(3).index();
    t = t.wrapping_add(S274::new(274 as u64).score()); let _ = S274::new(3).index();
    t = t.wrapping_add(S275::new(275 as u64).score()); let _ = S275::new(3).index();
    t = t.wrapping_add(S276::new(276 as u64).score()); let _ = S276::new(3).index();
    t = t.wrapping_add(S277::new(277 as u64).score()); let _ = S277::new(3).index();
    t = t.wrapping_add(S278::new(278 as u64).score()); let _ = S278::new(3).index();
    t = t.wrapping_add(S279::new(279 as u64).score()); let _ = S279::new(3).index();
    t = t.wrapping_add(S280::new(280 as u64).score()); let _ = S280::new(3).index();
    t = t.wrapping_add(S281::new(281 as u64).score()); let _ = S281::new(3).index();
    t = t.wrapping_add(S282::new(282 as u64).score()); let _ = S282::new(3).index();
    t = t.wrapping_add(S283::new(283 as u64).score()); let _ = S283::new(3).index();
    t = t.wrapping_add(S284::new(284 as u64).score()); let _ = S284::new(3).index();
    t = t.wrapping_add(S285::new(285 as u64).score()); let _ = S285::new(3).index();
    t = t.wrapping_add(S286::new(286 as u64).score()); let _ = S286::new(3).index();
    t = t.wrapping_add(S287::new(287 as u64).score()); let _ = S287::new(3).index();
    t = t.wrapping_add(S288::new(288 as u64).score()); let _ = S288::new(3).index();
    t = t.wrapping_add(S289::new(289 as u64).score()); let _ = S289::new(3).index();
    t = t.wrapping_add(S290::new(290 as u64).score()); let _ = S290::new(3).index();
    t = t.wrapping_add(S291::new(291 as u64).score()); let _ = S291::new(3).index();
    t = t.wrapping_add(S292::new(292 as u64).score()); let _ = S292::new(3).index();
    t = t.wrapping_add(S293::new(293 as u64).score()); let _ = S293::new(3).index();
    t = t.wrapping_add(S294::new(294 as u64).score()); let _ = S294::new(3).index();
    t = t.wrapping_add(S295::new(295 as u64).score()); let _ = S295::new(3).index();
    t = t.wrapping_add(S296::new(296 as u64).score()); let _ = S296::new(3).index();
    t = t.wrapping_add(S297::new(297 as u64).score()); let _ = S297::new(3).index();
    t = t.wrapping_add(S298::new(298 as u64).score()); let _ = S298::new(3).index();
    t = t.wrapping_add(S299::new(299 as u64).score()); let _ = S299::new(3).index();
    t = t.wrapping_add(S300::new(300 as u64).score()); let _ = S300::new(3).index();
    t = t.wrapping_add(S301::new(301 as u64).score()); let _ = S301::new(3).index();
    t = t.wrapping_add(S302::new(302 as u64).score()); let _ = S302::new(3).index();
    t = t.wrapping_add(S303::new(303 as u64).score()); let _ = S303::new(3).index();
    t = t.wrapping_add(S304::new(304 as u64).score()); let _ = S304::new(3).index();
    t = t.wrapping_add(S305::new(305 as u64).score()); let _ = S305::new(3).index();
    t = t.wrapping_add(S306::new(306 as u64).score()); let _ = S306::new(3).index();
    t = t.wrapping_add(S307::new(307 as u64).score()); let _ = S307::new(3).index();
    t = t.wrapping_add(S308::new(308 as u64).score()); let _ = S308::new(3).index();
    t = t.wrapping_add(S309::new(309 as u64).score()); let _ = S309::new(3).index();
    t = t.wrapping_add(S310::new(310 as u64).score()); let _ = S310::new(3).index();
    t = t.wrapping_add(S311::new(311 as u64).score()); let _ = S311::new(3).index();
    t = t.wrapping_add(S312::new(312 as u64).score()); let _ = S312::new(3).index();
    t = t.wrapping_add(S313::new(313 as u64).score()); let _ = S313::new(3).index();
    t = t.wrapping_add(S314::new(314 as u64).score()); let _ = S314::new(3).index();
    t = t.wrapping_add(S315::new(315 as u64).score()); let _ = S315::new(3).index();
    t = t.wrapping_add(S316::new(316 as u64).score()); let _ = S316::new(3).index();
    t = t.wrapping_add(S317::new(317 as u64).score()); let _ = S317::new(3).index();
    t = t.wrapping_add(S318::new(318 as u64).score()); let _ = S318::new(3).index();
    t = t.wrapping_add(S319::new(319 as u64).score()); let _ = S319::new(3).index();
    t = t.wrapping_add(S320::new(320 as u64).score()); let _ = S320::new(3).index();
    t = t.wrapping_add(S321::new(321 as u64).score()); let _ = S321::new(3).index();
    t = t.wrapping_add(S322::new(322 as u64).score()); let _ = S322::new(3).index();
    t = t.wrapping_add(S323::new(323 as u64).score()); let _ = S323::new(3).index();
    t = t.wrapping_add(S324::new(324 as u64).score()); let _ = S324::new(3).index();
    t = t.wrapping_add(S325::new(325 as u64).score()); let _ = S325::new(3).index();
    t = t.wrapping_add(S326::new(326 as u64).score()); let _ = S326::new(3).index();
    t = t.wrapping_add(S327::new(327 as u64).score()); let _ = S327::new(3).index();
    t = t.wrapping_add(S328::new(328 as u64).score()); let _ = S328::new(3).index();
    t = t.wrapping_add(S329::new(329 as u64).score()); let _ = S329::new(3).index();
    t = t.wrapping_add(S330::new(330 as u64).score()); let _ = S330::new(3).index();
    t = t.wrapping_add(S331::new(331 as u64).score()); let _ = S331::new(3).index();
    t = t.wrapping_add(S332::new(332 as u64).score()); let _ = S332::new(3).index();
    t = t.wrapping_add(S333::new(333 as u64).score()); let _ = S333::new(3).index();
    t = t.wrapping_add(S334::new(334 as u64).score()); let _ = S334::new(3).index();
    t = t.wrapping_add(S335::new(335 as u64).score()); let _ = S335::new(3).index();
    t = t.wrapping_add(S336::new(336 as u64).score()); let _ = S336::new(3).index();
    t = t.wrapping_add(S337::new(337 as u64).score()); let _ = S337::new(3).index();
    t = t.wrapping_add(S338::new(338 as u64).score()); let _ = S338::new(3).index();
    t = t.wrapping_add(S339::new(339 as u64).score()); let _ = S339::new(3).index();
    t = t.wrapping_add(S340::new(340 as u64).score()); let _ = S340::new(3).index();
    t = t.wrapping_add(S341::new(341 as u64).score()); let _ = S341::new(3).index();
    t = t.wrapping_add(S342::new(342 as u64).score()); let _ = S342::new(3).index();
    t = t.wrapping_add(S343::new(343 as u64).score()); let _ = S343::new(3).index();
    t = t.wrapping_add(S344::new(344 as u64).score()); let _ = S344::new(3).index();
    t = t.wrapping_add(S345::new(345 as u64).score()); let _ = S345::new(3).index();
    t = t.wrapping_add(S346::new(346 as u64).score()); let _ = S346::new(3).index();
    t = t.wrapping_add(S347::new(347 as u64).score()); let _ = S347::new(3).index();
    t = t.wrapping_add(S348::new(348 as u64).score()); let _ = S348::new(3).index();
    t = t.wrapping_add(S349::new(349 as u64).score()); let _ = S349::new(3).index();
    t = t.wrapping_add(S350::new(350 as u64).score()); let _ = S350::new(3).index();
    t = t.wrapping_add(S351::new(351 as u64).score()); let _ = S351::new(3).index();
    t = t.wrapping_add(S352::new(352 as u64).score()); let _ = S352::new(3).index();
    t = t.wrapping_add(S353::new(353 as u64).score()); let _ = S353::new(3).index();
    t = t.wrapping_add(S354::new(354 as u64).score()); let _ = S354::new(3).index();
    t = t.wrapping_add(S355::new(355 as u64).score()); let _ = S355::new(3).index();
    t = t.wrapping_add(S356::new(356 as u64).score()); let _ = S356::new(3).index();
    t = t.wrapping_add(S357::new(357 as u64).score()); let _ = S357::new(3).index();
    t = t.wrapping_add(S358::new(358 as u64).score()); let _ = S358::new(3).index();
    t = t.wrapping_add(S359::new(359 as u64).score()); let _ = S359::new(3).index();
    t = t.wrapping_add(S360::new(360 as u64).score()); let _ = S360::new(3).index();
    t = t.wrapping_add(S361::new(361 as u64).score()); let _ = S361::new(3).index();
    t = t.wrapping_add(S362::new(362 as u64).score()); let _ = S362::new(3).index();
    t = t.wrapping_add(S363::new(363 as u64).score()); let _ = S363::new(3).index();
    t = t.wrapping_add(S364::new(364 as u64).score()); let _ = S364::new(3).index();
    t = t.wrapping_add(S365::new(365 as u64).score()); let _ = S365::new(3).index();
    t = t.wrapping_add(S366::new(366 as u64).score()); let _ = S366::new(3).index();
    t = t.wrapping_add(S367::new(367 as u64).score()); let _ = S367::new(3).index();
    t = t.wrapping_add(S368::new(368 as u64).score()); let _ = S368::new(3).index();
    t = t.wrapping_add(S369::new(369 as u64).score()); let _ = S369::new(3).index();
    t = t.wrapping_add(S370::new(370 as u64).score()); let _ = S370::new(3).index();
    t = t.wrapping_add(S371::new(371 as u64).score()); let _ = S371::new(3).index();
    t = t.wrapping_add(S372::new(372 as u64).score()); let _ = S372::new(3).index();
    t = t.wrapping_add(S373::new(373 as u64).score()); let _ = S373::new(3).index();
    t = t.wrapping_add(S374::new(374 as u64).score()); let _ = S374::new(3).index();
    t = t.wrapping_add(S375::new(375 as u64).score()); let _ = S375::new(3).index();
    t = t.wrapping_add(S376::new(376 as u64).score()); let _ = S376::new(3).index();
    t = t.wrapping_add(S377::new(377 as u64).score()); let _ = S377::new(3).index();
    t = t.wrapping_add(S378::new(378 as u64).score()); let _ = S378::new(3).index();
    t = t.wrapping_add(S379::new(379 as u64).score()); let _ = S379::new(3).index();
    t = t.wrapping_add(S380::new(380 as u64).score()); let _ = S380::new(3).index();
    t = t.wrapping_add(S381::new(381 as u64).score()); let _ = S381::new(3).index();
    t = t.wrapping_add(S382::new(382 as u64).score()); let _ = S382::new(3).index();
    t = t.wrapping_add(S383::new(383 as u64).score()); let _ = S383::new(3).index();
    t = t.wrapping_add(S384::new(384 as u64).score()); let _ = S384::new(3).index();
    t = t.wrapping_add(S385::new(385 as u64).score()); let _ = S385::new(3).index();
    t = t.wrapping_add(S386::new(386 as u64).score()); let _ = S386::new(3).index();
    t = t.wrapping_add(S387::new(387 as u64).score()); let _ = S387::new(3).index();
    t = t.wrapping_add(S388::new(388 as u64).score()); let _ = S388::new(3).index();
    t = t.wrapping_add(S389::new(389 as u64).score()); let _ = S389::new(3).index();
    t = t.wrapping_add(S390::new(390 as u64).score()); let _ = S390::new(3).index();
    t = t.wrapping_add(S391::new(391 as u64).score()); let _ = S391::new(3).index();
    t = t.wrapping_add(S392::new(392 as u64).score()); let _ = S392::new(3).index();
    t = t.wrapping_add(S393::new(393 as u64).score()); let _ = S393::new(3).index();
    t = t.wrapping_add(S394::new(394 as u64).score()); let _ = S394::new(3).index();
    t = t.wrapping_add(S395::new(395 as u64).score()); let _ = S395::new(3).index();
    t = t.wrapping_add(S396::new(396 as u64).score()); let _ = S396::new(3).index();
    t = t.wrapping_add(S397::new(397 as u64).score()); let _ = S397::new(3).index();
    t = t.wrapping_add(S398::new(398 as u64).score()); let _ = S398::new(3).index();
    t = t.wrapping_add(S399::new(399 as u64).score()); let _ = S399::new(3).index();
    println!("{}", t);
}