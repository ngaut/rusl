use std::io;
use std::collections::HashMap;
use std::io::prelude::*;
use std::fs::File;
use std::env;

mod util;
mod lexer;
mod parser;
mod anf;

use util::get_unique_varname;

use lexer::Token;
use lexer::LexerState;

use parser::SExpr;
use parser::read;

use anf::Flat;
use anf::flatten;


#[derive(Debug, Clone)]
enum CC {
    // condition codes
    E, L, LE, G, GE,
}

#[derive(Debug, Clone)]
enum Reg {
    RAX, RBX, RBP
}

#[derive(Debug, Clone)]
enum X86Arg {
    Reg(Reg),
    Imm(i32),
    RegOffset(Reg, i32),
    Var(String),     // pseudo-x86
}

#[derive(Debug, Clone)]
enum X86 {
    Mov(X86Arg, X86Arg),
    Add(X86Arg, X86Arg),
    Cmp(X86Arg, X86Arg),
    EqP(X86Arg, X86Arg),          // pseudo-X86
    If(Box<X86>, Vec<X86>, Vec<X86>), // pseudo-X86
    Prog(Vec<X86>, Vec<String>),
    JmpIf(CC, String),
    Jmp(String),
    Label(String),
}



// uniquify variable names. This function simply adds a monotonically
// increasing counter(VAR_COUNTER) to each and every variable.
fn uniquify(mapping: &mut HashMap<String, String>, expr: SExpr) 
            -> SExpr {
    match expr {
        SExpr::Symbol(name) => 
            SExpr::Symbol(mapping.get(&name).unwrap().to_string()),
        SExpr::Number(_) => expr,
        SExpr::Bool(_) => expr,
        SExpr::Let(bindings, body) => {
            let mut new_bindings = vec![];
            for (k,v) in bindings {
                let uniq_k = get_unique_varname(&k);
                mapping.insert(k.clone(), uniq_k.clone());
                new_bindings.push((uniq_k, v));
            }
            return SExpr::Let(new_bindings, Box::new(uniquify(mapping, *body)));
        },
        SExpr::List(elts) => {
            let mut new_elts = vec![];
            for e in elts {
                new_elts.push(uniquify(mapping, e));
            }
            
            return SExpr::List(new_elts);
        }
        SExpr::Define(name, val) => {
            return SExpr::Define(name,
                                 Box::new(uniquify(mapping, *val)));
        },
        SExpr::If(cond, thn, els) => {
            return SExpr::If(Box::new(uniquify(mapping, *cond)),
                             Box::new(uniquify(mapping, *thn)),
                             Box::new(uniquify(mapping, *els)));
        },
        SExpr::App(f, args) => {
            let mut new_args = vec![];
            for a in args {
                new_args.push(uniquify(mapping, a));
            }
            return SExpr::App(f, new_args);
        },
        SExpr::Prog(e) =>
            return SExpr::Prog(Box::new(uniquify(mapping, *e))),
        SExpr::EOF => panic!("Don't know what to do with EOF"),
    }
}




fn flat_arg_type(v: Flat) -> X86Arg {
    match v {
        Flat::Symbol(name) => X86Arg::Var(name),
        Flat::Number(n) => X86Arg::Imm(n),
        Flat::Bool(b) => {
            match b {
                true => X86Arg::Imm(1),
                false => X86Arg::Imm(0),
            }
        },
        _ => panic!("flat_arg_type: compound expression"),
    }
}

// convert one Flat instruction to pseudo-x86
fn flat_to_px86(instr: Flat) -> Vec<X86> {
    match instr {
        Flat::Assign(dest, e) => {
            match *e {
                Flat::Symbol(name) => vec![X86::Mov(X86Arg::Var(dest), X86Arg::Var(name))],
                Flat::Number(n) => vec![X86::Mov(X86Arg::Var(dest), X86Arg::Imm(n))],
                Flat::Bool(b) => {
                    let bval = match b {
                        true => 1,
                        false => 0,
                    };
                    return vec![X86::Mov(X86Arg::Var(dest), 
                                         X86Arg::Imm(bval))];
                },
                // https://github.com/rust-lang/rust/issues/16223
                x => match x {
                    Flat::Prim(f, args) => {
                        match &f[..] {
                            "+" => {
                                // TODO: check arg count
                                let arg1 = args[0].clone();
                                let arg2 = args[1].clone();
                                return vec![
                                    X86::Mov(X86Arg::Var(dest.clone()),
                                             flat_arg_type(arg1)),
                                    X86::Add(X86Arg::Var(dest),
                                             flat_arg_type(arg2))
                                ];
                            },
                            _ => panic!("primitive not defined"),
                        }
                    },
                    _ => {
                        println!("{:?}", x);
                        panic!("NYI")
                    },
                },
            }
        },
        Flat::Return(v) => {
            let val = flat_arg_type(*v);
            return vec![X86::Mov(X86Arg::Reg(Reg::RAX), 
                                 val)]
        },
        Flat::If(cnd, thn, els) => {
            let (eq_left, eq_right) = match *cnd {
                x => match x {
                    Flat::EqP(left, right) => (left, right),
                    _ => panic!("if cond needs to be Flat::EqP"),
                },
            };
            let mut thn_instrs = vec![];
            for i in thn {
                let mut i_instrs = flat_to_px86(i);
                thn_instrs.append(&mut i_instrs);
            }
            let mut els_instrs = vec![];
            for i in els {
                let mut i_instrs = flat_to_px86(i);
                els_instrs.append(&mut i_instrs);
            }
            return vec![X86::If(Box::new(X86::EqP(flat_arg_type(*eq_left),
                                                  flat_arg_type(*eq_right))),
                                thn_instrs,
                                els_instrs)];
        },
        _ => panic!("NYI"),
    }
}

// convert a Flat expression into pseudo-x86 instructions. pseudo-x86
// is like x86 but with if's and temporaries. It is also
// "unpatched" (see `patch_instructions`)
fn select_instructions(flat_prog: Flat, prog_assigns: Vec<Flat>, prog_vars: Vec<String>) -> X86 {
    match flat_prog {
        Flat::Symbol(flat) => {
            match &flat[..] {
                "<PROGRAM>" => {
                    let mut x86_instrs = vec![];
                    for i in prog_assigns {
                        let mut i_instrs = flat_to_px86(i);
                        x86_instrs.append(&mut i_instrs);
                    }
                    return X86::Prog(x86_instrs, prog_vars);
                },
                &_ => panic!("arg passed to select_instructions is not top-level prog"),
            }
        },
        _ => panic!("flat_prog is not a symbol"),
    }
}

fn assign_homes_to_op2(locs: &HashMap<String, i32>, 
                       src: X86Arg, dest: X86Arg) -> (X86Arg, X86Arg) {
    match (dest.clone(), src.clone()) {
        (X86Arg::Var(d), X86Arg::Var(s)) =>
            (X86Arg::RegOffset(Reg::RBP, 
                               locs.get(&d).unwrap().clone()),
             X86Arg::RegOffset(Reg::RBP, 
                               locs.get(&s).unwrap().clone())),
        (X86Arg::Var(d), _) =>
            (X86Arg::RegOffset(Reg::RBP, 
                               locs.get(&d).unwrap().clone()),
             src) ,
        (_, X86Arg::Var(s)) =>
            (dest,
             X86Arg::RegOffset(Reg::RBP, 
                               locs.get(&s).unwrap().clone())) ,
        (X86Arg::Reg(reg), _) =>
            (dest, src) ,
        _ => panic!("unreachable"),
    }
}

fn assign_homes_to_instrs(instrs: Vec<X86>, locs: HashMap<String, i32>) -> Vec<X86> {
    let mut new_instrs = vec![];
    for i in instrs {
        match i {
            X86::If(cnd, thn, els) => {
                let new_cnd = match *cnd {
                    x => match x {
                        X86::EqP(left, right) => {
                        let new_left = match left {
                            X86Arg::Var(v) => 
                                X86Arg::RegOffset(Reg::RBP, 
                                                  locs.get(&v).unwrap().clone()),
                            _ => left,
                        };
                        X86::EqP(new_left, right)
                        },
                        _ => panic!("if cond should be an EqP"),
                    },
                };
                let new_thn = assign_homes_to_instrs(thn, locs.clone());
                let new_els = assign_homes_to_instrs(els, locs.clone());
                new_instrs.push(
                    X86::If(Box::new(new_cnd), new_thn, new_els)
                );
            },
            X86::Mov(dest, src) => {
                let (new_dest, new_src) = assign_homes_to_op2(&locs, src, dest);
                new_instrs.push(X86::Mov(new_dest, new_src))
            },
            X86::Add(dest, src) => {
                let (new_dest, new_src) = assign_homes_to_op2(&locs, src, dest);
                new_instrs.push(X86::Add(new_dest, new_src))
            },
            _ => panic!("NYI"),
        }
    };
    
    return new_instrs;
}

fn assign_homes(prog: X86) -> X86 {
    match prog {
        X86::Prog(instrs, vars) => {
            let mut locs = HashMap::new();
            let mut stack_size = 0;
            for var in vars.clone() {
                stack_size += 1;
                locs.insert(var, stack_size * -8);
            };
            return X86::Prog(assign_homes_to_instrs(instrs, locs), vars);
        },
        _ => panic!("assign_homes: not top level prog"),
    }
}

fn lower_if (instr: X86) -> Vec<X86> {
    match instr {
        X86::If(cnd, thn, els) => {
            let (eqp_left, eqp_right) = match *cnd {
                x => match x {
                    X86::EqP(left, right) => (left, right),
                    _ => panic!("if cond is always EqP"),
                },
            };
            let thn_label = get_unique_varname("then");
            let end_label = get_unique_varname("endif");

            let mut new_elss = vec![];
            for i in els {
                new_elss.extend_from_slice(&lower_if(i));
            }
            let mut new_thns = vec![];
            for i in thn {
                new_thns.extend_from_slice(&lower_if(i));
            }

            let mut if_instrs = vec![
                X86::Cmp(eqp_right, eqp_left),
                X86::JmpIf(CC::E, thn_label.clone()),
            ];
            if_instrs.append(&mut new_elss);
            if_instrs.extend_from_slice(&[
                X86::Jmp(end_label.clone()),
                X86::Label(thn_label),
            ]);
            if_instrs.append(&mut new_thns);
            if_instrs.extend_from_slice(&[
                X86::Label(end_label),
            ]);

            return if_instrs;
        },
        _ => vec![instr],
    }
}

fn lower_conditionals(prog: X86) -> X86 {
    match prog {
        X86::Prog(instrs, vars) => {
            let mut new_instrs = vec![];
            for i in instrs {
                new_instrs.extend_from_slice(&lower_if(i));
            }

            return X86::Prog(new_instrs, vars);
        }
        _ => panic!("lower_conditionals: not top-level Prog"),
    }
}

fn patch_single_instr(instr: X86) -> Vec<X86> {
    match instr {
        X86::Mov(X86Arg::RegOffset(Reg::RBP, dest), 
                 X86Arg::RegOffset(Reg::RBP, src)) => {
            vec![X86::Mov(X86Arg::Reg(Reg::RAX),
                          X86Arg::RegOffset(Reg::RBP, src)),
                 X86::Mov(X86Arg::RegOffset(Reg::RBP, dest), 
                          X86Arg::Reg(Reg::RAX))]
        },
        X86::Add(X86Arg::RegOffset(Reg::RBP, dest), 
                 X86Arg::RegOffset(Reg::RBP, src)) => {
            vec![X86::Mov(X86Arg::Reg(Reg::RAX),
                          X86Arg::RegOffset(Reg::RBP, dest)),
                 X86::Add(X86Arg::Reg(Reg::RAX),
                          X86Arg::RegOffset(Reg::RBP, src)),
                 X86::Mov(X86Arg::RegOffset(Reg::RBP, dest),
                          X86Arg::Reg(Reg::RAX))
            ]
        },
        _ => vec![instr],
    }
}

fn patch_instructions(prog: X86) -> X86 {
    match prog {
        X86::Prog(instrs, vars) => {
            let mut patched_instrs = vec![];
            for i in instrs {
                patched_instrs.extend_from_slice(&patch_single_instr(i));
            }
            return X86::Prog(patched_instrs, vars);
        },
        _ => panic!("patch_instructions: not top-level Prog"),
    }
}


fn display_reg(reg: Reg) -> String {
    match reg {
        Reg::RAX => "rax",
        Reg::RBX => "rbx",
        Reg::RBP => "rbp",
    }.to_string()
}

fn print_x86_arg(arg: X86Arg) -> String {
    match arg {
        X86Arg::Reg(r) => format!("{}", display_reg(r)),
        X86Arg::Imm(n) => format!("{}", n),
        X86Arg::RegOffset(r, offset) => format!("QWORD [{}{}]", 
                                                display_reg(r), offset),
        _ => panic!("invalid arg type"),
    }
}

fn print_CC(cc: CC) -> String {
    match cc {
        CC::E => "e", 
        CC::L => "l", 
        CC::LE => "le", 
        CC::G => "g", 
        CC::GE => "ge",
    }.to_string()
}

fn print_instr(instr: X86) -> String {
    let instr_string = match instr.clone() {
        X86::Mov(dest, src) => format!("mov {}, {}", 
                                       print_x86_arg(dest), 
                                       print_x86_arg(src)),
        X86::Add(dest, src) => format!("add {}, {}", 
                                       print_x86_arg(dest), 
                                       print_x86_arg(src)),
        X86::Cmp(left, right) => format!("cmp {}, {}",
                                        print_x86_arg(left), 
                                        print_x86_arg(right)),
        X86::JmpIf(cc, label) => format!("j{} {}",
                                         print_CC(cc),
                                         label),
        X86::Jmp(label) => format!("jmp {}", label),
        X86::Label(label) => format!("{}:", label),

        _ => panic!("invalid op"),
    };

    match instr {
        X86::Label(_) => return format!("{}\n", instr_string),
        _ => return format!("    {}\n", instr_string),
    }
}

fn print_x86(prog: X86) -> String {
    let prelude = "section .text
extern print_int
global main
main:
    push rbp
    mov rbp, rsp\n";
    // TODO: save/restore registers
    let postlude = "    mov rdi, rax
    call print_int

    mov rsp, rbp
    pop rbp
    ret\n";
    let mut instrs_str = match prog {
        X86::Prog(instrs, vars) => {
            let mut instrs_str = String::from(prelude);
            for i in instrs {
                instrs_str.push_str(&print_instr(i));
            }
            instrs_str
        },
        _ => panic!("print_x86: not top-level Prog"),
    };

    instrs_str.push_str(postlude);
    return instrs_str;
}


fn read_input() -> io::Result<()> {
    let args : Vec<_> = env::args().collect();
    if args.len() < 2 {
        panic!("usage: {} filename", args[0].clone());
    }

    let mut f = try!(File::open(args[1].clone()));
    let mut input = String::new();
    try!(f.read_to_string(&mut input));

    let mut lexer = LexerState {
        s: input,
        pos: 0,                 // absolute position
        col: 1,                 // column in line
        line_num: 1,
        tok_buf: None,
    };

    let mut uniquify_mapping = HashMap::new();

    let (flat_prog, prog_assigns, prog_vars) =
        flatten(uniquify(&mut uniquify_mapping,
                         SExpr::Prog(Box::new(read(&mut lexer)))));

    println!("{}", print_x86(patch_instructions(lower_conditionals(assign_homes(select_instructions(flat_prog, prog_assigns, prog_vars))))));

    Ok(())
}

fn main() {
    read_input();
}
