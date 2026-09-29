//! A tiny assembler for classic BPF programs.
//!
//! loot-ledger attaches a filter to its capture socket so the kernel discards
//! everything that is not Albion traffic *before* copying a frame to user
//! space. Without it, a machine streaming video would copy every frame into
//! our process just for us to throw it away.
//!
//! Classic BPF is a tiny instruction set — a load, a compare-and-branch, a
//! return — and encoding it by hand means raw integers scattered through the
//! source. This module turns the filter into something readable instead.

/// Instruction fields, as laid out by `struct sock_filter`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction {
    /// Operation and modifier bits.
    pub code: u16,
    /// Instructions to skip when the comparison is true.
    pub jt: u8,
    /// Instructions to skip when the comparison is false.
    pub jf: u8,
    /// Constant operand, or the byte offset for a load.
    pub k: u32,
}

const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_H: u16 = 0x08;
const BPF_B: u16 = 0x10;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;

// Combined opcodes. These must be separate `const` items rather than inline
// `A | B | C` expressions: in pattern position Rust reads `|` as "or", so an
// inline expression would silently become three alternative binding patterns
// and never match.
const OP_LD_H_ABS: u16 = BPF_LD | BPF_H | BPF_ABS;
const OP_LD_B_ABS: u16 = BPF_LD | BPF_B | BPF_ABS;
const OP_JMP_JEQ: u16 = BPF_JMP | BPF_JEQ;
const OP_RET: u16 = BPF_RET;

const ETH_P_IP: u32 = 0x0800;
const IPPROTO_UDP: u32 = 17;

/// Accept value: a non-zero return tells the kernel to deliver the frame.
/// The kernel caps the returned length at the socket's snaplen either way.
const ACCEPT: u32 = 0xffff;

/// Accumulates instructions and resolves forward jump targets.
///
/// Branches are emitted with placeholder offsets and patched once the target
/// instruction index is known, which keeps the control flow readable in source
/// order.
#[derive(Debug, Default)]
pub struct Program {
    instructions: Vec<Instruction>,
}

impl Program {
    /// An empty program.
    pub fn new() -> Self {
        Program::default()
    }

    /// `ldh [k]` — load a big-endian 16-bit word at absolute offset `k`.
    pub fn load_half(&mut self, offset: u32) -> &mut Self {
        self.push(Instruction {
            code: OP_LD_H_ABS,
            jt: 0,
            jf: 0,
            k: offset,
        });
        self
    }

    /// `ldb [k]` — load a byte at absolute offset `k`.
    pub fn load_byte(&mut self, offset: u32) -> &mut Self {
        self.push(Instruction {
            code: OP_LD_B_ABS,
            jt: 0,
            jf: 0,
            k: offset,
        });
        self
    }

    /// `jeq #k, ?` — compare the accumulator against a constant.
    ///
    /// Returns the index of the emitted branch so it can be patched once its
    /// destinations are known.
    pub fn jump_if_equal(&mut self, k: u32) -> usize {
        self.push(Instruction {
            code: OP_JMP_JEQ,
            jt: 0,
            jf: 0,
            k,
        })
    }

    /// `ret #0xffff` — deliver the frame to user space.
    pub fn accept(&mut self) -> usize {
        self.push(Instruction {
            code: OP_RET,
            jt: 0,
            jf: 0,
            k: ACCEPT,
        })
    }

    /// `ret #0` — drop the frame.
    ///
    /// Named `reject` rather than `drop` so it cannot be confused with the
    /// standard library's `Drop::drop`.
    pub fn reject(&mut self) {
        self.push(Instruction {
            code: OP_RET,
            jt: 0,
            jf: 0,
            k: 0,
        });
    }

    /// Send the *matching* outcome of every branch in `at` to `target`.
    ///
    /// The non-matching outcome is left alone, so the branch falls through to
    /// the next instruction, which is how the port chain advances.
    pub fn patch_true(&mut self, at: &[usize], target: usize) {
        for &i in at {
            self.instructions[i].jt = Self::delta(i, target);
        }
    }

    /// Send the *non-matching* outcome of every branch in `at` to `target`.
    pub fn patch_false(&mut self, at: &[usize], target: usize) {
        for &i in at {
            self.instructions[i].jf = Self::delta(i, target);
        }
    }

    /// Branch offsets are relative to the instruction *after* the branch.
    fn delta(at: usize, target: usize) -> u8 {
        let d = target as i64 - (at as i64 + 1);
        assert!(
            (0..=255).contains(&d),
            "BPF jump offset {d} out of range at instruction {at}; \
             the filter needs restructuring"
        );
        d as u8
    }

    /// Number of instructions emitted so far; the index the next one will take.
    pub fn here(&self) -> usize {
        self.instructions.len()
    }

    /// Finish the program.
    pub fn build(self) -> Vec<Instruction> {
        self.instructions
    }

    fn push(&mut self, ins: Instruction) -> usize {
        self.instructions.push(ins);
        self.instructions.len() - 1
    }
}

/// Frame-relative byte offsets used by the filter.
///
/// Classic BPF addresses bytes from the very start of the frame, so every
/// IP-header offset needs the 14-byte Ethernet header added. Getting this
/// wrong is silent: the filter still installs, it just never matches anything.
mod off {
    /// etherType, immediately after the 6-byte destination and source MACs.
    pub const ETHERTYPE: u32 = 12;
    /// IPv4 protocol byte: 14 (Ethernet) + 9 (its offset within the IP header).
    pub const IP_PROTOCOL: u32 = 14 + 9;
    /// UDP source port: 14 (Ethernet) + 20 (its offset within the IP header).
    pub const UDP_SRC_PORT: u32 = 14 + 20;
    /// UDP destination port: 14 (Ethernet) + 22 (its offset within the IP header).
    pub const UDP_DST_PORT: u32 = 14 + 22;
}

/// Build the filter that keeps only Albion's game traffic.
///
/// Matches an Ethernet frame carrying IPv4/UDP to or from any of `ports`.
///
/// Shape:
///
/// ```text
///     ethertype == IPv4?
///     ip protocol == UDP?
///     src port in ports? ──────────────> accept
///     dst port in ports? ──────────────> accept
///                                    \-> drop
/// ```
pub fn albion_filter(ports: &[u16]) -> Vec<Instruction> {
    let mut p = Program::new();

    // 1. Only IPv4, and only UDP.
    //
    // The matching outcome of each test falls through to the next stage; the
    // non-matching outcome jumps to the single reject at the end, so the
    // success path has to be laid out first.
    p.load_half(off::ETHERTYPE);
    let is_ipv4 = p.jump_if_equal(ETH_P_IP);
    p.load_byte(off::IP_PROTOCOL);
    let is_udp = p.jump_if_equal(IPPROTO_UDP);

    // 2. Source port. The loaded value stays in the accumulator, so only the
    //    first comparison of the chain needs its own load.
    let mut port_branches: Vec<usize> = Vec::with_capacity(ports.len() * 2);

    p.load_half(off::UDP_SRC_PORT);
    for port in ports {
        port_branches.push(p.jump_if_equal(*port as u32));
    }

    // 3. Destination port, same shape. A non-matching frame falls straight
    //    through the end of this chain into the reject below.
    p.load_half(off::UDP_DST_PORT);
    for port in ports {
        port_branches.push(p.jump_if_equal(*port as u32));
    }

    // 4. Outcomes, laid out so every failure can reach one reject.
    let reject = p.here();
    p.reject();

    let accept = p.here();
    p.accept();

    p.patch_true(&port_branches, accept);
    p.patch_false(&[is_ipv4, is_udp], reject);

    p.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    thread_local! {
        static ACC: Cell<i64> = const { Cell::new(0) };
    }

    /// Interpret a program against a frame, the way the kernel would.
    fn run(prog: &[Instruction], frame: &[u8]) -> u32 {
        let mut pc = 0usize;
        for _ in 0..10_000 {
            let i = &prog[pc];
            match i.code {
                OP_LD_H_ABS => {
                    let k = i.k as usize;
                    assert!(k + 1 < frame.len(), "load past end of frame at {k}");
                    let v = u16::from_be_bytes([frame[k], frame[k + 1]]);
                    ACC.with(|a| a.set(v as i64));
                    pc += 1;
                }
                OP_LD_B_ABS => {
                    let k = i.k as usize;
                    assert!(k < frame.len(), "load past end of frame at {k}");
                    ACC.with(|a| a.set(frame[k] as i64));
                    pc += 1;
                }
                OP_JMP_JEQ => {
                    let acc = ACC.with(|a| a.get());
                    pc += if acc == i.k as i64 {
                        1 + i.jt as usize
                    } else {
                        1 + i.jf as usize
                    };
                }
                OP_RET => return i.k,
                other => panic!("unexpected opcode 0x{other:04x}"),
            }
            assert!(pc < prog.len(), "program ran off the end");
        }
        panic!("program did not terminate");
    }

    /// Build an Ethernet + IPv4 + UDP frame.
    fn frame(src_port: u16, dst_port: u16, protocol: u8) -> Vec<u8> {
        let mut f = vec![0xaa; 12];
        f.extend_from_slice(&0x0800u16.to_be_bytes());
        f.extend_from_slice(&[0x45, 0x00]);
        f.extend_from_slice(&[0x00, 40]);
        f.extend_from_slice(&[0x00, 0x00]);
        f.extend_from_slice(&[0x40, 0x00]);
        f.extend_from_slice(&[64, protocol]);
        f.extend_from_slice(&[0x00, 0x00]);
        f.extend_from_slice(&[10, 0, 0, 1]);
        f.extend_from_slice(&[10, 0, 0, 2]);
        f.extend_from_slice(&src_port.to_be_bytes());
        f.extend_from_slice(&dst_port.to_be_bytes());
        f.extend_from_slice(&[0x00, 24]);
        f.extend_from_slice(&[0x00, 0x00]);
        f
    }

    #[test]
    fn accepts_albion_ports_in_either_direction() {
        let prog = albion_filter(&[5056, 5055, 4535]);
        assert_eq!(run(&prog, &frame(5056, 12345, 17)), ACCEPT);
        assert_eq!(run(&prog, &frame(12345, 5056, 17)), ACCEPT);
        assert_eq!(run(&prog, &frame(5055, 12345, 17)), ACCEPT);
        assert_eq!(run(&prog, &frame(12345, 4535, 17)), ACCEPT);
    }

    #[test]
    fn drops_other_udp() {
        let prog = albion_filter(&[5056, 5055, 4535]);
        assert_eq!(run(&prog, &frame(53, 5353, 17)), 0);
    }

    #[test]
    fn drops_other_ip_protocols() {
        let prog = albion_filter(&[5056, 5055, 4535]);
        assert_eq!(run(&prog, &frame(5056, 5056, 6)), 0); // TCP
    }

    #[test]
    fn drops_non_ipv4_ethertypes() {
        let prog = albion_filter(&[5056, 5055, 4535]);
        let mut f = frame(5056, 5056, 17);
        f[12..14].copy_from_slice(&0x86ddu16.to_be_bytes()); // ARP
        assert_eq!(run(&prog, &f), 0);
    }

    #[test]
    fn every_jump_lands_on_a_real_instruction() {
        // A mis-patched jump is the classic way to hand the kernel a program
        // it rejects with EINVAL, which shows up as a silent "no packets".
        let prog = albion_filter(&[5056, 5055, 4535]);
        for (i, ins) in prog.iter().enumerate() {
            if ins.code == OP_JMP_JEQ {
                assert!(
                    (i + 1 + ins.jt as usize) < prog.len(),
                    "true branch at {i} runs past the program"
                );
                assert!(
                    (i + 1 + ins.jf as usize) < prog.len(),
                    "false branch at {i} runs past the program"
                );
            }
        }
    }

    #[test]
    fn program_fits_the_kernel_limit() {
        // Linux caps a BPF program at 4096 instructions.
        let prog = albion_filter(&[5056, 5055, 4535]);
        assert!(!prog.is_empty());
        assert!(prog.len() < 4096);
    }

    #[test]
    fn single_port_filter_works() {
        let prog = albion_filter(&[5056]);
        assert_eq!(run(&prog, &frame(5056, 1, 17)), ACCEPT);
        assert_eq!(run(&prog, &frame(1, 5056, 17)), ACCEPT);
        assert_eq!(run(&prog, &frame(1, 2, 17)), 0);
    }
}
