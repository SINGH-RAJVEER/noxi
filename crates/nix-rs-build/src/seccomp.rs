//! The syscall filter CppNix installs in the sandbox (`setupSeccomp`):
//! builders may not create setuid or setgid files, and extended attribute
//! calls fail with `ENOTSUP`, for the native and 32-bit x86 ABIs.

use std::io;

const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_JMP_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;

/// Offsets into `struct seccomp_data`.
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const fn offset_arg_low(i: u32) -> u32 {
	16 + 8 * i
}

#[derive(Clone, Copy)]
enum Rule {
	/// Fail with EPERM when the mode argument sets setuid or setgid.
	SetidMode { nr: u32, arg: u32 },
	/// Always fail with ENOTSUP.
	NoXattr { nr: u32 },
}

fn rules(chmod: [u32; 4], xattrs: [u32; 9]) -> Vec<Rule> {
	let [chmod_nr, fchmod, fchmodat, fchmodat2] = chmod;
	let mut rules = vec![
		Rule::SetidMode { nr: chmod_nr, arg: 1 },
		Rule::SetidMode { nr: fchmod, arg: 1 },
		Rule::SetidMode { nr: fchmodat, arg: 2 },
		Rule::SetidMode { nr: fchmodat2, arg: 2 },
	];
	rules.extend(xattrs.map(|nr| Rule::NoXattr { nr }));
	rules
}

fn insn(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
	libc::sock_filter { code, jt, jf, k }
}

fn program() -> Vec<libc::sock_filter> {
	// chmod, fchmod, fchmodat, fchmodat2; then setxattr, lsetxattr,
	// fsetxattr, getxattr, lgetxattr, fgetxattr, listxattr, llistxattr,
	// flistxattr.
	let x86_64 = rules([90, 91, 268, 452], [188, 189, 190, 191, 192, 193, 194, 195, 196]);
	let x32 = rules(
		[90, 91, 268, 452].map(|n| n | X32_SYSCALL_BIT),
		[188, 189, 190, 191, 192, 193, 194, 195, 196].map(|n| n | X32_SYSCALL_BIT),
	);
	let i386 = rules([15, 94, 306, 452], [226, 227, 228, 229, 230, 231, 232, 233, 234]);

	let mut prog = Vec::new();
	for (arch, rules) in [(AUDIT_ARCH_X86_64, x86_64), (AUDIT_ARCH_X86_64, x32), (AUDIT_ARCH_I386, i386)] {
		for rule in rules {
			match rule {
				Rule::SetidMode { nr, arg } => {
					// 7 instructions; each failed test jumps past the rest,
					// including the final return.
					prog.push(insn(BPF_LD_W_ABS, 0, 0, OFFSET_ARCH));
					prog.push(insn(BPF_JMP_JEQ_K, 0, 5, arch));
					prog.push(insn(BPF_LD_W_ABS, 0, 0, OFFSET_NR));
					prog.push(insn(BPF_JMP_JEQ_K, 0, 3, nr));
					prog.push(insn(BPF_LD_W_ABS, 0, 0, offset_arg_low(arg)));
					prog.push(insn(BPF_JMP_JSET_K, 0, 1, (libc::S_ISUID | libc::S_ISGID) as u32));
					prog.push(insn(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32));
				}
				Rule::NoXattr { nr } => {
					prog.push(insn(BPF_LD_W_ABS, 0, 0, OFFSET_ARCH));
					prog.push(insn(BPF_JMP_JEQ_K, 0, 3, arch));
					prog.push(insn(BPF_LD_W_ABS, 0, 0, OFFSET_NR));
					prog.push(insn(BPF_JMP_JEQ_K, 0, 1, nr));
					prog.push(insn(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::ENOTSUP as u32));
				}
			}
		}
	}
	prog.push(insn(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW));
	prog
}

/// Install the filter in the calling process.
pub fn install() -> io::Result<()> {
	let prog = program();
	let fprog = libc::sock_fprog {
		len: prog.len() as u16,
		filter: prog.as_ptr() as *mut libc::sock_filter,
	};
	// SAFETY: `fprog` points to `prog`, which outlives the call.
	let ret = unsafe {
		libc::prctl(
			libc::PR_SET_SECCOMP,
			libc::SECCOMP_MODE_FILTER as libc::c_ulong,
			&fprog as *const libc::sock_fprog as libc::c_ulong,
			0,
			0,
		)
	};
	if ret == -1 {
		let error = io::Error::last_os_error();
		return Err(io::Error::new(error.kind(), format!("unable to load seccomp BPF program: {error}")));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Run `prog` on a syscall as the kernel would, without installing it.
	fn verdict(prog: &[libc::sock_filter], arch: u32, nr: u32, args: [u64; 6]) -> u32 {
		let word = |offset: u32| match offset {
			OFFSET_NR => nr,
			OFFSET_ARCH => arch,
			offset => args[(offset as usize - 16) / 8] as u32,
		};
		let (mut pc, mut acc) = (0, 0);
		loop {
			let insn = prog[pc];
			pc += 1;
			match insn.code {
				BPF_LD_W_ABS => acc = word(insn.k),
				BPF_JMP_JEQ_K | BPF_JMP_JSET_K => {
					let taken = if insn.code == BPF_JMP_JEQ_K { acc == insn.k } else { acc & insn.k != 0 };
					pc += usize::from(if taken { insn.jt } else { insn.jf });
				}
				BPF_RET_K => return insn.k,
				code => panic!("unexpected instruction {code:#x}"),
			}
		}
	}

	#[test]
	fn filters_only_the_listed_calls() {
		let prog = program();
		let eperm = SECCOMP_RET_ERRNO | libc::EPERM as u32;
		let enotsup = SECCOMP_RET_ERRNO | libc::ENOTSUP as u32;
		let setuid = u64::from(libc::S_ISUID);
		let x86_64 = |nr, args| verdict(&prog, AUDIT_ARCH_X86_64, nr, args);
		// read, and chmod and fchmodat without setuid or setgid.
		assert_eq!(x86_64(0, [0; 6]), SECCOMP_RET_ALLOW);
		assert_eq!(x86_64(90, [0, 0o755, 0, 0, 0, 0]), SECCOMP_RET_ALLOW);
		assert_eq!(x86_64(268, [0, 0, 0o644, 0, 0, 0]), SECCOMP_RET_ALLOW);
		assert_eq!(x86_64(90, [0, setuid | 0o755, 0, 0, 0, 0]), eperm);
		assert_eq!(x86_64(268, [0, 0, u64::from(libc::S_ISGID), 0, 0, 0]), eperm);
		assert_eq!(x86_64(188, [0; 6]), enotsup);
		assert_eq!(x86_64(90 | X32_SYSCALL_BIT, [0, setuid, 0, 0, 0, 0]), eperm);
		// i386 numbers: chmod is 15, while 90 is mmap there.
		let i386 = |nr, args| verdict(&prog, AUDIT_ARCH_I386, nr, args);
		assert_eq!(i386(90, [0, setuid, 0, 0, 0, 0]), SECCOMP_RET_ALLOW);
		assert_eq!(i386(15, [0, setuid, 0, 0, 0, 0]), eperm);
		assert_eq!(i386(226, [0; 6]), enotsup);
	}
}
