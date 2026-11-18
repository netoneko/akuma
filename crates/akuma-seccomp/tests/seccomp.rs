use akuma_seccomp::{
    ALLOW, AUDIT_ARCH_AARCH64, AUDIT_ARCH_AMD64, Action, Data, ERRNO, Instruction, InvalidProgram,
    KILL_PROCESS, KILL_THREAD, LOG, MAX_INSNS, Program, TRACE, TRAP, Verdict,
};

fn ins(code: u16, k: u32) -> Instruction {
    Instruction {
        code,
        k,
        jt: 0,
        jf: 0,
    }
}

fn data(nr: i32, args: [u64; 6]) -> Data {
    Data {
        nr,
        arch: AUDIT_ARCH_AMD64,
        instruction_pointer: 0x0123_4567_89ab_cdef,
        args,
    }
}

fn reject(code: &[Instruction], error: InvalidProgram) {
    assert_eq!(Program::new(code.to_vec()).err(), Some(error));
}

#[test]
fn validates_the_linux_instruction_subset() {
    let ret = ins(6, ALLOW);
    reject(&[], InvalidProgram::Length);
    assert!(Program::new(vec![ret; MAX_INSNS]).is_ok());
    reject(&vec![ret; MAX_INSNS + 1], InvalidProgram::Length);
    reject(&[ins(0x9c, 1), ret], InvalidProgram::Opcode); // MOD X
    reject(&[ins(0x28, 0), ret], InvalidProgram::Opcode); // halfword load
    reject(&[ins(0x40, 0), ret], InvalidProgram::Opcode); // indirect load
    reject(&[ins(0x106, 0), ret], InvalidProgram::Opcode);
    reject(&[ins(0x20, 2), ret], InvalidProgram::Operand);
    reject(&[ins(0x20, 64), ret], InvalidProgram::Operand);
    reject(&[ins(0x34, 0), ret], InvalidProgram::Operand);
    reject(&[ins(0x74, 32), ret], InvalidProgram::Operand);
    reject(&[ins(5, u32::MAX), ret], InvalidProgram::Jump);
    reject(&[ins(5, 1), ret], InvalidProgram::Jump);
    reject(
        &[
            Instruction {
                jt: 1,
                ..ins(0x15, 0)
            },
            ret,
        ],
        InvalidProgram::Jump,
    );
    reject(&[ret, ins(0, 1)], InvalidProgram::Return);
}

#[test]
fn scratch_reads_require_writes_on_every_incoming_branch() {
    let ret = ins(6, ALLOW);
    let load = ins(0x60, 15);
    let store = ins(2, 15);
    reject(&[load, ret], InvalidProgram::Scratch);
    reject(
        &[
            Instruction {
                jf: 1,
                ..ins(0x15, 0)
            },
            store,
            load,
            ret,
        ],
        InvalidProgram::Scratch,
    );
    assert!(Program::new(vec![store, ins(5, 0), load, ret]).is_ok());
    assert!(Program::new(vec![ins(5, 1), load, ret]).is_ok());
    reject(&[ins(5, 1), load, load, ret], InvalidProgram::Scratch);
    reject(&[ret, load, ret], InvalidProgram::Scratch);
}

#[test]
fn evaluates_registers_arithmetic_and_data() {
    let d = data(42, [0, 0, 0, 0, 0, 0xFEDC_BA98_7654_3210]);
    let eval = |code: &[Instruction]| Program::new(code.to_vec()).unwrap().evaluate(&d);
    assert_eq!(eval(&[ins(0x16, 0)]), 0);
    assert_eq!(eval(&[ins(0x81, 0), ins(0x87, 0), ins(0x16, 0)]), 64);
    if cfg!(target_endian = "little") {
        assert_eq!(eval(&[ins(0x20, 8), ins(0x16, 0)]), 0x89ab_cdef);
        assert_eq!(eval(&[ins(0x20, 60), ins(0x16, 0)]), 0xfedc_ba98);
    }
    assert_eq!(
        eval(&[ins(0, 1), ins(1, 33), ins(0x6c, 0), ins(0x16, 0)]),
        2
    );
    assert_eq!(eval(&[ins(0, 1), ins(0x3c, 0), ins(6, ALLOW)]), KILL_THREAD);
    assert_eq!(
        eval(&[
            ins(0, u32::MAX),
            ins(0x04, 1),
            ins(2, 0),
            ins(0x61, 0),
            ins(0x87, 0),
            ins(0x16, 0)
        ]),
        0
    );
}

#[test]
fn action_precedence_and_newest_ties() {
    let order = [
        (KILL_PROCESS, Action::KillProcess),
        (KILL_THREAD, Action::KillThread),
        (TRAP | 0x2a, Action::Trap(42)),
        (ERRNO | 1, Action::Errno(1)),
        (TRACE, Action::Trace),
        (LOG, Action::Log),
        (ALLOW, Action::Allow),
    ];
    for (i, (winner, action)) in order.into_iter().enumerate() {
        for &(loser, _) in &order[i..] {
            for stack in [[winner, loser], [loser, winner]] {
                let mut v = Verdict::ALLOW;
                for raw in stack {
                    v.consider(raw);
                }
                assert_eq!(v.action(), action);
            }
        }
    }
    let mut v = Verdict::ALLOW;
    v.consider(ERRNO | 0x1388);
    v.consider(ERRNO | 1);
    assert_eq!(v.action(), Action::Errno(4095));
    let mut v = Verdict::ALLOW;
    v.consider(ERRNO);
    assert_eq!(v.action(), Action::Errno(0));
    let mut v = Verdict::ALLOW;
    v.consider(0x1234_0000);
    assert_eq!(v.action(), Action::KillProcess);
}

#[test]
fn codex_amd64_filter_captured_on_linux() {
    let raw = include_bytes!("fixtures/codex-0.169.0-amd64.cbpf");
    let code = raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| Instruction {
            code: u16::from_le_bytes([b[0], b[1]]),
            jt: b[2],
            jf: b[3],
            k: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        })
        .collect();
    let program = Program::new(code).unwrap();
    assert_eq!(program.instruction_count(), 114);
    assert_eq!(program.evaluate(&data(0, [0; 6])), ALLOW); // read
    assert_eq!(program.evaluate(&data(59, [0; 6])), ALLOW); // execve
    assert_eq!(program.evaluate(&data(41, [2, 1, 0, 0, 0, 0])), ERRNO | 1); // AF_INET
    assert_eq!(program.evaluate(&data(41, [1, 1, 0, 0, 0, 0])), ALLOW); // AF_UNIX
    assert_ne!(
        program.evaluate(&Data {
            arch: AUDIT_ARCH_AARCH64,
            ..data(0, [0; 6])
        }),
        ALLOW
    );
}
