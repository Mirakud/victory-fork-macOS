//! A DXIL module and its LLVM 3.7 bitcode, written with unabbreviated
//! records only (the format DXIL fixes; codes from LLVM's `LLVMBitCodes.h`).
//!
//! Values are symbolic while the module is built ([`V`]) and numbered when
//! it is written: functions, then constants (all at module level), then the
//! instructions of `main` in block order.

use std::collections::HashMap;

pub mod census;
mod input_reads;
pub mod opt;

pub(crate) use input_reads::{
    PixelBodyInventory, PixelInputRead, PixelInputType, pixel_body_inventory, pixel_input_reads,
};

fn pixel_input_opcode(name: &str) -> Option<u32> {
    if name.starts_with("dx.op.loadInput.") {
        Some(4)
    } else if name.starts_with("dx.op.attributeAtVertex.") {
        Some(137)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Ty(u32);

/// What the lowering made an instruction for, where it says so (counted by
/// [`census`], nothing else reads it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    /// The select that keeps a register's old value in lanes a guard masks.
    Guard,
    /// An index clamp the translation adds (the console has none).
    Clamp,
    /// The iteration counter and limit test the translation adds to a loop.
    Counter,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TypeDef {
    Void,
    Int(u32),
    Float,
    Double,
    /// Pointee and address space.
    Ptr(Ty, u32),
    Struct(Option<String>, Vec<Ty>),
    Array(u32, Ty),
    Func(Ty, Vec<Ty>),
}

/// A value: a global variable, a function, a module constant, or an
/// instruction result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum V {
    Global(u32),
    Func(u32),
    Const(u32),
    Inst(u32),
}

/// A global variable declaration (thread-group shared memory).
struct Global {
    name: String,
    ty: Ty,
    /// Its pointer type.
    ptr: Ty,
    space: u32,
    /// An internal constant with this initialiser (a constant table).
    init: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Const {
    Int(i64),
    Float(u32),
    Undef,
    Aggregate(Vec<u32>),
}

/// LLVM binary opcodes (the same codes serve the float forms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bin {
    Add = 0,
    Sub = 1,
    Mul = 2,
    UDiv = 3,
    SDiv = 4,
    URem = 5,
    Shl = 7,
    LShr = 8,
    AShr = 9,
    And = 10,
    Or = 11,
    Xor = 12,
}

/// `atomicrmw` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rmw {
    Xchg = 0,
    Add = 1,
    Sub = 2,
    And = 3,
    Or = 5,
    Xor = 6,
    Max = 7,
    Min = 8,
    UMax = 9,
    UMin = 10,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cast {
    Trunc = 0,
    ZExt = 1,
    SExt = 2,
    FpToUi = 3,
    FpToSi = 4,
    UiToFp = 5,
    SiToFp = 6,
    Bitcast = 11,
}

/// Compare predicates: floats 0-15, integers 32-41.
pub(crate) mod pred {
    pub const FALSE: u8 = 0;
    pub const OEQ: u8 = 1;
    pub const OGT: u8 = 2;
    pub const OGE: u8 = 3;
    pub const OLT: u8 = 4;
    pub const OLE: u8 = 5;
    // Not emitted: the AMD D3D12 driver treats it as UNE, so `v_cmp_lg`
    // lowers as OLT | OGT instead.
    #[allow(dead_code)]
    pub const ONE: u8 = 6;
    pub const ORD: u8 = 7;
    pub const UNO: u8 = 8;
    pub const UEQ: u8 = 9;
    pub const UGT: u8 = 10;
    pub const UGE: u8 = 11;
    pub const ULT: u8 = 12;
    pub const ULE: u8 = 13;
    pub const UNE: u8 = 14;
    pub const TRUE: u8 = 15;
    pub const EQ: u8 = 32;
    pub const NE: u8 = 33;
    pub const IUGT: u8 = 34;
    pub const IUGE: u8 = 35;
    pub const IULT: u8 = 36;
    pub const IULE: u8 = 37;
    pub const SGT: u8 = 38;
    pub const SGE: u8 = 39;
    pub const SLT: u8 = 40;
    pub const SLE: u8 = 41;
}

#[derive(Debug, Clone)]
enum Op {
    Bin(Bin, V, V),
    Cast(Cast, V),
    Cmp(u8, V, V),
    Select(V, V, V),
    Extract(V, u32),
    Call(u32, Vec<V>),
    /// Filled in later: (value, block).
    Phi(Vec<(V, usize)>),
    /// `getelementptr` over the pointee type, from a pointer.
    Gep(Ty, V, Vec<V>),
    Load(V),
    /// Pointer, value.
    Store(V, V),
    AtomicRmw(Rmw, V, V),
    /// Pointer, expected, new; returns `{i32, i1}`.
    CmpXchg(V, V, V),
}

#[derive(Debug, Clone)]
struct Inst {
    ty: Ty,
    op: Op,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Term {
    Br(usize),
    CondBr(V, usize, usize),
    Ret,
}

#[derive(Debug, Clone, Default)]
struct Block {
    phis: Vec<u32>,
    body: Vec<u32>,
    term: Option<Term>,
}

/// Function attributes of a `dx.op` declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attr {
    ReadNone = 1,
    NoUnwind = 2,
    ReadOnly = 3,
    /// Barriers.
    NoDuplicate = 4,
}

struct Func {
    name: String,
    ty: Ty,
    attr: Option<Attr>,
    body: bool,
}

/// A metadata node operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Md(u32);

enum MdDef {
    Str(String),
    Value(Ty, V),
    Node(Vec<Option<Md>>),
}

#[derive(Default)]
pub(crate) struct Module {
    types: Vec<TypeDef>,
    type_ids: HashMap<TypeDef, u32>,
    consts: Vec<(Ty, Const)>,
    const_ids: HashMap<(Ty, Const), u32>,
    globals: Vec<Global>,
    funcs: Vec<Func>,
    func_ids: HashMap<String, u32>,
    insts: Vec<Inst>,
    blocks: Vec<Block>,
    mds: Vec<MdDef>,
    named: Vec<(String, Vec<Md>)>,
    /// See [`Tag`]: by instruction.
    tags: HashMap<u32, Tag>,
}

pub(crate) enum MeshVaryingStage {
    MeshOutput,
    PixelInput,
}

#[derive(Clone, Copy)]
pub(crate) enum MeshVaryingSlot {
    Remap(u32, u8),
    RemovedClip,
    OutsideWidth,
}

impl Module {
    fn ty(&mut self, def: TypeDef) -> Ty {
        if let Some(&t) = self.type_ids.get(&def) {
            return Ty(t);
        }
        let t = self.types.len() as u32;
        self.types.push(def.clone());
        self.type_ids.insert(def, t);
        Ty(t)
    }

    pub fn void(&mut self) -> Ty {
        self.ty(TypeDef::Void)
    }
    pub fn int(&mut self, bits: u32) -> Ty {
        self.ty(TypeDef::Int(bits))
    }
    pub fn float(&mut self) -> Ty {
        self.ty(TypeDef::Float)
    }
    pub fn double(&mut self) -> Ty {
        self.ty(TypeDef::Double)
    }
    pub fn ptr(&mut self, to: Ty) -> Ty {
        self.ty(TypeDef::Ptr(to, 0))
    }
    pub fn ptr_in(&mut self, to: Ty, space: u32) -> Ty {
        self.ty(TypeDef::Ptr(to, space))
    }
    pub fn array(&mut self, n: u32, of: Ty) -> Ty {
        self.ty(TypeDef::Array(n, of))
    }
    pub fn named_struct(&mut self, name: &str, fields: Vec<Ty>) -> Ty {
        self.ty(TypeDef::Struct(Some(name.into()), fields))
    }
    pub fn func_ty(&mut self, ret: Ty, params: Vec<Ty>) -> Ty {
        self.ty(TypeDef::Func(ret, params))
    }

    fn is_void(&self, t: Ty) -> bool {
        self.types[t.0 as usize] == TypeDef::Void
    }

    fn constant(&mut self, ty: Ty, c: Const) -> V {
        if let Some(&i) = self.const_ids.get(&(ty, c.clone())) {
            return V::Const(i);
        }
        let i = self.consts.len() as u32;
        self.consts.push((ty, c.clone()));
        self.const_ids.insert((ty, c), i);
        V::Const(i)
    }

    pub fn int_const(&mut self, ty: Ty, v: i64) -> V {
        self.constant(ty, Const::Int(v))
    }
    pub fn i32(&mut self, v: u32) -> V {
        let t = self.int(32);
        self.int_const(t, i64::from(v as i32))
    }
    pub fn i8(&mut self, v: u8) -> V {
        let t = self.int(8);
        self.int_const(t, i64::from(v as i8))
    }
    pub fn bool(&mut self, v: bool) -> V {
        let t = self.int(1);
        self.int_const(t, -i64::from(v))
    }
    pub fn f32(&mut self, bits: u32) -> V {
        let t = self.float();
        self.constant(t, Const::Float(bits))
    }
    pub fn undef(&mut self, ty: Ty) -> V {
        self.constant(ty, Const::Undef)
    }
    /// A constant struct or array of constants.
    pub fn aggregate(&mut self, ty: Ty, fields: &[V]) -> V {
        let ids = fields
            .iter()
            .map(|f| match f {
                V::Const(i) => *i,
                _ => panic!("aggregate of a non-constant"),
            })
            .collect();
        self.constant(ty, Const::Aggregate(ids))
    }

    /// The value of an integer constant; `None` for anything else.
    pub fn int_value(&self, v: V) -> Option<i64> {
        match v {
            V::Const(i) => match self.consts[i as usize].1 {
                Const::Int(x) => Some(x),
                _ => None,
            },
            _ => None,
        }
    }

    /// Gives constant `old` the content of `new` (both stay valid).
    pub fn replace_const(&mut self, old: V, new: V) {
        if let (V::Const(o), V::Const(n)) = (old, new) {
            self.consts[o as usize] = self.consts[n as usize].clone();
        }
    }

    /// Rewrites the lowered calls that address one signature by element and relative column.
    /// Only an explicitly removed, neutral clip output may be discarded.
    pub(crate) fn remap_mesh_varying_calls(
        &mut self,
        stage: MeshVaryingStage,
        map: &[[MeshVaryingSlot; 4]],
    ) -> Result<(), String> {
        let integer = |v, bits| {
            let V::Const(index) = v else { return None };
            let (ty, Const::Int(value)) = self.consts[index as usize] else {
                return None;
            };
            if self.types[ty.0 as usize] != TypeDef::Int(bits) {
                return None;
            }
            u32::try_from(value).ok()
        };
        let mut rewrites = Vec::new();
        let mut discarded = vec![false; self.insts.len()];
        // Validate the whole module before changing a call or interning a constant.
        for &i in self.blocks.iter().flat_map(|block| &block.body) {
            let inst = &self.insts[i as usize];
            let Op::Call(func, args) = &inst.op else {
                continue;
            };
            let name = &self.funcs[*func as usize].name;
            let (intrinsic, opcode, arity) = match stage {
                MeshVaryingStage::MeshOutput if name.starts_with("dx.op.storeVertexOutput.") => {
                    ("storeVertexOutput", 171, 6)
                }
                MeshVaryingStage::PixelInput => {
                    let Some(opcode) = pixel_input_opcode(name) else {
                        continue;
                    };
                    let intrinsic = if opcode == 4 {
                        "loadInput"
                    } else {
                        "attributeAtVertex"
                    };
                    (intrinsic, opcode, 5)
                }
                _ => continue,
            };
            let reject = |reason| format!("{intrinsic}: {reason}");
            if args.len() != arity {
                return Err(reject("unexpected argument count"));
            }
            if integer(args[0], 32) != Some(opcode) {
                return Err(reject("unexpected opcode operand"));
            }
            match integer(args[2], 32) {
                Some(0) => {}
                Some(_) => return Err(reject("nonzero relative row")),
                None => return Err(reject("relative row is not a nonnegative i32 constant")),
            }
            let id = integer(args[1], 32)
                .ok_or_else(|| reject("element is not a nonnegative i32 constant"))?;
            let col = integer(args[3], 8)
                .ok_or_else(|| reject("column is not a nonnegative i8 constant"))?;
            let new = map
                .get(id as usize)
                .ok_or_else(|| reject("element is outside the remap"))?
                .get(col as usize)
                .ok_or_else(|| reject("relative column is outside the remap"))?;
            match *new {
                MeshVaryingSlot::Remap(new_id, new_col) if new_col < 4 => {
                    rewrites.push((i, new_id, new_col));
                }
                MeshVaryingSlot::Remap(..) => {
                    return Err(reject("destination column is outside the remap"));
                }
                MeshVaryingSlot::OutsideWidth => {
                    return Err(reject("relative column is outside the declared element width"));
                }
                MeshVaryingSlot::RemovedClip => {
                    if opcode != 171 {
                        return Err(reject("reads a removed clip component"));
                    }
                    if !self.is_void(inst.ty) {
                        return Err(reject("clip store does not return void"));
                    }
                    if name != "dx.op.storeVertexOutput.f32" {
                        return Err(reject("clip store does not have the f32 overload"));
                    }
                    self.check_neutral_clip_value(args[4]).map_err(reject)?;
                    discarded[i as usize] = true;
                }
            }
        }
        for (i, id, col) in rewrites {
            let (id, col) = (self.i32(id), self.i8(col));
            let Op::Call(_, args) = &mut self.insts[i as usize].op else {
                unreachable!()
            };
            args[1] = id;
            args[3] = col;
        }
        for block in &mut self.blocks {
            block.body.retain(|&i| !discarded[i as usize]);
        }
        if matches!(stage, MeshVaryingStage::MeshOutput) {
            self.prepare_repacked_vertex_stores();
        }
        Ok(())
    }

    // Remapping can join channels from separate guest exports. Metal merges
    // the new element at its first store, so the later export's converted
    // values must exist there as well. Only move pure calculations across
    // vertex output stores; retain both orders and stop at other operations.
    fn prepare_repacked_vertex_stores(&mut self) {
        let store_or_calculation = |id: &u32| match &self.insts[*id as usize].op {
            Op::Cast(..) | Op::Bin(..) => Some(false),
            Op::Call(func, _) => {
                let name = self.funcs[*func as usize].name.as_str();
                if name.starts_with("dx.op.storeVertexOutput.") {
                    Some(true)
                } else if name == "dx.op.legacyF16ToF32" {
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        };
        for block in &mut self.blocks {
            for run in block.body.split_mut(|id| store_or_calculation(id).is_none()) {
                run.sort_by_key(|id| store_or_calculation(id) == Some(true));
            }
        }
    }

    fn check_neutral_clip_value(&self, v: V) -> Result<(), &'static str> {
        if self.types[self.type_of(v).0 as usize] != TypeDef::Float {
            return Err("clip value is not f32");
        }
        match v {
            V::Const(i) => match self.consts[i as usize].1 {
                Const::Float(0) => Ok(()),
                Const::Float(_) => Err("clip value is a non-neutral f32 literal"),
                Const::Undef => Err("clip value is undefined"),
                _ => Err("clip value has an unexpected constant form"),
            },
            V::Inst(i) => match self.insts[i as usize].op {
                Op::Cast(Cast::Bitcast, bits) => {
                    if self.types[self.type_of(bits).0 as usize] != TypeDef::Int(32) {
                        return Err("clip value is a bitcast from a type other than i32");
                    }
                    match self.int_value(bits) {
                        Some(0) => Ok(()),
                        Some(_) => Err("clip value is a bitcast of a non-neutral i32 literal"),
                        None => match bits {
                            V::Inst(i) => match self.insts[i as usize].op {
                                Op::Phi(_) => Err("clip value is a bitcast of a phi result"),
                                Op::Select(..) => Err("clip value is a bitcast of a select result"),
                                Op::Cast(Cast::Bitcast, _) => Err("clip value is a nested bitcast"),
                                _ => Err("clip value is a bitcast of another instruction result"),
                            },
                            V::Const(i) if self.consts[i as usize].1 == Const::Undef => {
                                Err("clip value is a bitcast of an undefined i32")
                            }
                            _ => Err("clip value is a bitcast of an unexpected operand"),
                        },
                    }
                }
                Op::Phi(_) => Err("clip value is a phi result"),
                Op::Select(..) => Err("clip value is a select result"),
                Op::Cast(..) => Err("clip value is a conversion result"),
                _ => Err("clip value is another instruction result"),
            },
            _ => Err("clip value has an unexpected operand form"),
        }
    }

    /// A function declaration (or `main` with `body`), found by name.
    pub fn function(&mut self, name: &str, ty: Ty, attr: Option<Attr>, body: bool) -> V {
        if let Some(&i) = self.func_ids.get(name) {
            return V::Func(i);
        }
        let i = self.funcs.len() as u32;
        self.funcs.push(Func {
            name: name.into(),
            ty,
            attr,
            body,
        });
        self.func_ids.insert(name.into(), i);
        V::Func(i)
    }

    /// An external global of type `ty` in address space `space` (DXIL
    /// declares thread-group shared memory this way).
    pub fn global(&mut self, name: &str, ty: Ty, space: u32) -> V {
        let ptr = self.ptr_in(ty, space);
        self.globals.push(Global {
            name: name.into(),
            ty,
            ptr,
            space,
            init: None,
        });
        V::Global(self.globals.len() as u32 - 1)
    }

    /// An internal constant global initialised with the constant `init`.
    pub fn constant_global(&mut self, name: &str, init: V) -> V {
        let V::Const(i) = init else {
            panic!("a global initialised with a non-constant")
        };
        let ty = self.consts[i as usize].0;
        let g = self.global(name, ty, 0);
        if let V::Global(n) = g {
            self.globals[n as usize].init = Some(i);
        }
        g
    }

    /// Notes what instruction `v` is for (see [`Tag`]); only when modules
    /// are counted.
    pub fn tag(&mut self, v: V, tag: Tag) {
        if let V::Inst(i) = v
            && census::enabled()
        {
            self.tags.insert(i, tag);
        }
    }

    pub fn new_block(&mut self) -> usize {
        self.blocks.push(Block::default());
        self.blocks.len() - 1
    }

    pub fn type_of(&self, v: V) -> Ty {
        match v {
            V::Global(i) => self.globals[i as usize].ptr,
            V::Const(i) => self.consts[i as usize].0,
            V::Inst(i) => self.insts[i as usize].ty,
            V::Func(i) => {
                let t = self.funcs[i as usize].ty;
                *self
                    .type_ids
                    .get(&TypeDef::Ptr(t, 0))
                    .map(|t| Ty(*t))
                    .as_ref()
                    .expect("function pointer type")
            }
        }
    }

    fn push(&mut self, block: usize, ty: Ty, op: Op) -> V {
        let i = self.insts.len() as u32;
        self.insts.push(Inst { ty, op });
        // Terminators are kept apart, so a block can still grow at its end.
        self.blocks[block].body.push(i);
        V::Inst(i)
    }

    pub fn bin(&mut self, block: usize, op: Bin, a: V, b: V) -> V {
        let ty = self.type_of(a);
        self.push(block, ty, Op::Bin(op, a, b))
    }
    pub fn cast(&mut self, block: usize, op: Cast, v: V, to: Ty) -> V {
        self.push(block, to, Op::Cast(op, v))
    }
    pub fn cmp(&mut self, block: usize, pred: u8, a: V, b: V) -> V {
        let t = self.int(1);
        self.push(block, t, Op::Cmp(pred, a, b))
    }
    pub fn select(&mut self, block: usize, c: V, t: V, f: V) -> V {
        let ty = self.type_of(t);
        self.push(block, ty, Op::Select(c, t, f))
    }
    pub fn extract(&mut self, block: usize, agg: V, index: u32) -> V {
        let ty = match &self.types[self.type_of(agg).0 as usize] {
            TypeDef::Struct(_, f) => f[index as usize],
            TypeDef::Array(_, t) => *t,
            t => panic!("extractvalue from {t:?}"),
        };
        self.push(block, ty, Op::Extract(agg, index))
    }
    pub fn call(&mut self, block: usize, f: V, args: Vec<V>) -> V {
        let V::Func(i) = f else {
            panic!("call of a non-function")
        };
        let TypeDef::Func(ret, _) = self.types[self.funcs[i as usize].ty.0 as usize] else {
            unreachable!()
        };
        self.push(block, ret, Op::Call(i, args))
    }
    /// A phi at the top of `block`; incoming values come with [`Module::set_phi`].
    pub fn phi(&mut self, block: usize, ty: Ty) -> V {
        let i = self.insts.len() as u32;
        self.insts.push(Inst {
            ty,
            op: Op::Phi(Vec::new()),
        });
        self.blocks[block].phis.push(i);
        V::Inst(i)
    }
    /// The address of element `index` of the array `ptr` points to.
    pub fn element(&mut self, block: usize, ptr: V, index: V) -> V {
        let TypeDef::Ptr(arr, space) = self.types[self.type_of(ptr).0 as usize] else {
            panic!("element of a non-pointer")
        };
        let TypeDef::Array(_, elem) = self.types[arr.0 as usize] else {
            panic!("element of a non-array")
        };
        let ty = self.ptr_in(elem, space);
        let zero = self.i32(0);
        self.push(block, ty, Op::Gep(arr, ptr, vec![zero, index]))
    }
    pub fn load(&mut self, block: usize, ptr: V) -> V {
        let TypeDef::Ptr(ty, _) = self.types[self.type_of(ptr).0 as usize] else {
            panic!("load from a non-pointer")
        };
        self.push(block, ty, Op::Load(ptr))
    }
    pub fn store(&mut self, block: usize, ptr: V, value: V) {
        let void = self.void();
        self.push(block, void, Op::Store(ptr, value));
    }
    pub fn atomic(&mut self, block: usize, op: Rmw, ptr: V, value: V) -> V {
        let ty = self.type_of(value);
        self.push(block, ty, Op::AtomicRmw(op, ptr, value))
    }
    /// The old value of a compare-exchange.
    pub fn cmpxchg(&mut self, block: usize, ptr: V, expected: V, new: V) -> V {
        let (i32t, i1) = (self.type_of(new), self.int(1));
        let pair = self.ty(TypeDef::Struct(None, vec![i32t, i1]));
        let r = self.push(block, pair, Op::CmpXchg(ptr, expected, new));
        self.extract(block, r, 0)
    }

    pub fn set_phi(&mut self, phi: V, incoming: Vec<(V, usize)>) {
        let V::Inst(i) = phi else { panic!() };
        self.insts[i as usize].op = Op::Phi(incoming);
    }
    pub fn terminate(&mut self, block: usize, t: Term) {
        self.blocks[block].term = Some(t);
    }

    pub fn md_str(&mut self, s: &str) -> Md {
        self.mds.push(MdDef::Str(s.into()));
        Md(self.mds.len() as u32 - 1)
    }
    pub fn md_value(&mut self, v: V) -> Md {
        let ty = self.type_of(v);
        self.mds.push(MdDef::Value(ty, v));
        Md(self.mds.len() as u32 - 1)
    }
    pub fn md_node(&mut self, ops: Vec<Option<Md>>) -> Md {
        self.mds.push(MdDef::Node(ops));
        Md(self.mds.len() as u32 - 1)
    }
    pub fn named_md(&mut self, name: &str, nodes: Vec<Md>) {
        self.named.push((name.into(), nodes));
    }

    /// The module as bitcode.
    /// Which instructions are written: every body instruction, the branch
    /// conditions, and whatever they read, transitively (a phi nothing reads
    /// is dropped). A worklist visits each instruction once, so a long chain
    /// of merges costs one pass, not one pass per merge.
    fn live(&self) -> Vec<bool> {
        let mut live = vec![false; self.insts.len()];
        let mut work: Vec<u32> = Vec::new();
        let mark = |live: &mut [bool], work: &mut Vec<u32>, v: V| {
            if let V::Inst(i) = v
                && !live[i as usize]
            {
                live[i as usize] = true;
                work.push(i);
            }
        };
        for b in &self.blocks {
            for &i in &b.body {
                mark(&mut live, &mut work, V::Inst(i));
            }
            if let Some(Term::CondBr(c, ..)) = b.term {
                mark(&mut live, &mut work, c);
            }
        }
        while let Some(i) = work.pop() {
            for_each_operand(&self.insts[i as usize].op, |v| {
                mark(&mut live, &mut work, v)
            });
        }
        live
    }

    pub fn write(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(b"BC\xC0\xDE");
        w.enter(MODULE_BLOCK, 3);
        w.record(1, &[1]); // VERSION: relative value ids

        // Attribute groups 1-4 and one list per group.
        w.enter(PARAMATTR_GROUP_BLOCK, 3);
        for (id, kinds) in [
            (1, &[18u64, 20][..]),
            (2, &[18]),
            (3, &[18, 21]),
            (4, &[12, 18]),
        ] {
            let mut r = vec![id, 0xFFFF_FFFF];
            for &k in kinds {
                r.extend([0, k]);
            }
            w.record(3, &r);
        }
        w.exit();
        w.enter(PARAMATTR_BLOCK, 3);
        for id in 1..=4 {
            w.record(2, &[id]);
        }
        w.exit();

        w.enter(TYPE_BLOCK, 4);
        w.record(1, &[self.types.len() as u64]);
        for t in &self.types {
            match t {
                TypeDef::Void => w.record(2, &[]),
                TypeDef::Float => w.record(3, &[]),
                TypeDef::Double => w.record(4, &[]),
                TypeDef::Int(b) => w.record(7, &[u64::from(*b)]),
                TypeDef::Ptr(p, space) => w.record(8, &[u64::from(p.0), u64::from(*space)]),
                TypeDef::Array(n, e) => w.record(11, &[u64::from(*n), u64::from(e.0)]),
                TypeDef::Struct(name, fields) => {
                    let mut r = vec![0];
                    r.extend(fields.iter().map(|f| u64::from(f.0)));
                    match name {
                        Some(n) => {
                            w.record(19, &chars(n));
                            w.record(20, &r);
                        }
                        None => w.record(18, &r),
                    }
                }
                TypeDef::Func(ret, params) => {
                    let mut r = vec![0, u64::from(ret.0)];
                    r.extend(params.iter().map(|p| u64::from(p.0)));
                    w.record(21, &r);
                }
            }
        }
        w.exit();

        w.record(2, &chars("dxil-ms-dx"));
        w.record(
            3,
            &chars(
                "e-m:e-p:32:32-i1:32-i8:32-i16:32-i32:32-i64:64-f16:32-f32:32-f64:64-n8:16:32:64",
            ),
        );
        let first_const = (self.globals.len() + self.funcs.len()) as u64;
        for g in &self.globals {
            // [type, address space << 2 | explicit type | constant, init id
            // + 1, linkage (external 0, internal 3), log2 alignment + 1,
            // section]
            let (constant, init, linkage) = match g.init {
                Some(i) => (1, first_const + u64::from(i) + 1, 3),
                None => (0, 0, 0),
            };
            w.record(
                7,
                &[
                    u64::from(g.ty.0),
                    u64::from(g.space) << 2 | 2 | constant,
                    init,
                    linkage,
                    3,
                    0,
                ],
            );
        }
        for f in &self.funcs {
            // [type, cc, isproto, linkage, paramattr, align, section,
            // visibility, gc, unnamed_addr, prologue, dllstorage, comdat, prefix]
            let attr = f.attr.map_or(0, |a| a as u64);
            w.record(
                8,
                &[
                    u64::from(f.ty.0),
                    0,
                    u64::from(!f.body),
                    0,
                    attr,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ],
            );
        }

        // Value ids: globals, functions, constants, instructions.
        let globals = self.globals.len() as u32;
        let funcs = globals + self.funcs.len() as u32;
        let const_id = |i: u32| u64::from(funcs + i);
        w.enter(CONSTANTS_BLOCK, 4);
        let mut current = None;
        for (ty, c) in &self.consts {
            if current != Some(*ty) {
                w.record(1, &[u64::from(ty.0)]);
                current = Some(*ty);
            }
            match c {
                Const::Undef => w.record(3, &[]),
                Const::Int(v) => w.record(4, &[signed(*v)]),
                Const::Float(bits) => w.record(6, &[u64::from(*bits)]),
                Const::Aggregate(ids) => {
                    let r: Vec<u64> = ids.iter().map(|&i| const_id(i)).collect();
                    w.record(7, &r);
                }
            }
        }
        w.exit();

        // Phis nothing reads are left out (one register per phi makes many).
        let live = self.live();

        // Blocks are laid out in reverse postorder, so every block follows
        // its forward predecessors (drivers take a branch to an earlier
        // block for a loop's back edge).
        let succs = |b: usize| successors(self.blocks[b].term);
        let mut seen = vec![false; self.blocks.len()];
        let mut post = Vec::new();
        let mut stack = vec![(0usize, 0usize)];
        seen[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            top.1 += 1;
            match succs(b).get(k) {
                Some(&s) if !seen[s] => {
                    seen[s] = true;
                    stack.push((s, 0));
                }
                Some(_) => {}
                None => {
                    post.push(b);
                    stack.pop();
                }
            }
        }
        post.reverse();
        let mut pos = vec![u64::MAX; self.blocks.len()];
        for (n, &b) in post.iter().enumerate() {
            pos[b] = n as u64;
        }
        // An empty latch (only a branch back to its loop header) is folded
        // into its one predecessor, so the exiting block is the latch as in
        // DXC's loops; the AMD driver hangs on divergent loops otherwise.
        let mut preds = vec![Vec::new(); self.blocks.len()];
        for &b in &post {
            for &s in succs(b).iter() {
                preds[s].push(b);
            }
        }
        let mut fold = vec![None; self.blocks.len()];
        for &b in &post {
            let blk = &self.blocks[b];
            if let (Some(Term::Br(h)), true, [p]) = (
                blk.term,
                blk.phis.is_empty() && blk.body.is_empty(),
                &preds[b][..],
            ) && pos[h] < pos[b]
                && !succs(*p).contains(&h)
            {
                fold[b] = Some((*p, h));
            }
        }
        let layout: Vec<usize> = post.into_iter().filter(|&b| fold[b].is_none()).collect();
        let mut pos = vec![u64::MAX; self.blocks.len()];
        for (n, &b) in layout.iter().enumerate() {
            pos[b] = n as u64;
        }
        // Branch targets and phi blocks through folded latches.
        let target = |t: usize| fold[t].map_or(pos[t], |(_, h)| pos[h]);
        let from = |b: usize| fold[b].map_or(pos[b], |(p, _)| pos[p]);

        // Instruction value ids, in layout order.
        let base = funcs + self.consts.len() as u32;
        let mut ids = vec![u32::MAX; self.insts.len()];
        let mut next = base;
        for b in layout.iter().map(|&b| &self.blocks[b]) {
            for &i in b.phis.iter().filter(|&&i| live[i as usize]).chain(&b.body) {
                if !self.is_void(self.insts[i as usize].ty) {
                    ids[i as usize] = next;
                    next += 1;
                }
            }
        }
        let abs = |v: V| -> u32 {
            match v {
                V::Global(i) => i,
                V::Func(i) => globals + i,
                V::Const(i) => funcs + i,
                V::Inst(i) => {
                    let id = ids[i as usize];
                    assert_ne!(id, u32::MAX, "use of a void value");
                    id
                }
            }
        };

        w.enter(METADATA_BLOCK, 3);
        for m in &self.mds {
            match m {
                MdDef::Str(s) => w.record(1, &chars(s)),
                MdDef::Value(ty, v) => w.record(2, &[u64::from(ty.0), u64::from(abs(*v))]),
                MdDef::Node(ops) => {
                    let r: Vec<u64> = ops
                        .iter()
                        .map(|o| o.map_or(0, |m| u64::from(m.0) + 1))
                        .collect();
                    w.record(3, &r);
                }
            }
        }
        for (name, nodes) in &self.named {
            w.record(4, &chars(name));
            let r: Vec<u64> = nodes.iter().map(|m| u64::from(m.0)).collect();
            w.record(10, &r);
        }
        w.exit();
        // The metadata kinds an LLVM 3.7 module declares, as DXC writes them.
        w.enter(METADATA_BLOCK, 3);
        for (id, kind) in [
            "dbg",
            "tbaa",
            "prof",
            "fpmath",
            "range",
            "tbaa.struct",
            "invariant.load",
            "alias.scope",
            "noalias",
            "nontemporal",
            "llvm.mem.parallel_loop_access",
            "nonnull",
            "dereferenceable",
            "dereferenceable_or_null",
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = vec![id as u64];
            r.extend(chars(kind));
            w.record(6, &r);
        }
        w.exit();

        w.enter(VALUE_SYMTAB_BLOCK, 4);
        let names = self.globals.iter().map(|g| &g.name);
        for (i, name) in names.chain(self.funcs.iter().map(|f| &f.name)).enumerate() {
            let mut r = vec![i as u64];
            r.extend(chars(name));
            w.record(1, &r);
        }
        w.exit();

        w.enter(FUNCTION_BLOCK, 4);
        w.record(1, &[layout.len() as u64]);
        let mut inst_id = base;
        // The operands of variable-length records, reused across them.
        let mut r: Vec<u64> = Vec::new();
        for b in layout.iter().map(|&b| &self.blocks[b]) {
            for &i in b.phis.iter().filter(|&&i| live[i as usize]).chain(&b.body) {
                let inst = &self.insts[i as usize];
                let rel = |v: V| {
                    let a = abs(v);
                    assert!(a < inst_id, "forward reference outside a phi");
                    u64::from(inst_id - a)
                };
                match &inst.op {
                    Op::Bin(op, a, b) => w.record(2, &[rel(*a), rel(*b), *op as u64]),
                    Op::Cast(op, v) => w.record(3, &[rel(*v), u64::from(inst.ty.0), *op as u64]),
                    Op::Cmp(p, a, b) => w.record(28, &[rel(*a), rel(*b), u64::from(*p)]),
                    Op::Select(c, t, f) => w.record(29, &[rel(*t), rel(*f), rel(*c)]),
                    Op::Extract(a, idx) => w.record(26, &[rel(*a), u64::from(*idx)]),
                    Op::Call(f, args) => {
                        r.clear();
                        r.extend([0, 1 << 15, u64::from(self.funcs[*f as usize].ty.0)]);
                        r.push(rel(V::Func(*f)));
                        r.extend(args.iter().map(|&a| rel(a)));
                        w.record(34, &r);
                    }
                    Op::Phi(incoming) => {
                        r.clear();
                        r.push(u64::from(inst.ty.0));
                        for &(v, b) in incoming {
                            r.push(signed(i64::from(inst_id) - i64::from(abs(v))));
                            r.push(from(b));
                        }
                        w.record(16, &r);
                    }
                    Op::Gep(ty, p, idx) => {
                        r.clear();
                        r.extend([0, u64::from(ty.0), rel(*p)]);
                        r.extend(idx.iter().map(|&i| rel(i)));
                        w.record(43, &r);
                    }
                    // Alignment 4 (log2 + 1), not volatile.
                    Op::Load(p) => w.record(20, &[rel(*p), u64::from(inst.ty.0), 3, 0]),
                    Op::Store(p, v) => w.record(44, &[rel(*p), rel(*v), 3, 0]),
                    // Sequentially consistent, across threads.
                    Op::AtomicRmw(op, p, v) => {
                        w.record(38, &[rel(*p), rel(*v), *op as u64, 0, 6, 1])
                    }
                    Op::CmpXchg(p, c, n) => {
                        w.record(46, &[rel(*p), rel(*c), rel(*n), 0, 6, 1, 6, 0]);
                    }
                }
                if !self.is_void(inst.ty) {
                    inst_id += 1;
                }
            }
            match b.term.expect("unterminated block") {
                Term::Ret => w.record(10, &[]),
                Term::Br(t) => w.record(11, &[target(t)]),
                Term::CondBr(c, t, f) => {
                    let a = abs(c);
                    assert!(a < inst_id);
                    w.record(11, &[target(t), target(f), u64::from(inst_id - a)]);
                }
            }
        }
        w.exit();
        w.exit();
        w.finish()
    }
}

const MODULE_BLOCK: u64 = 8;
const PARAMATTR_BLOCK: u64 = 9;
const PARAMATTR_GROUP_BLOCK: u64 = 10;
const CONSTANTS_BLOCK: u64 = 11;
const FUNCTION_BLOCK: u64 = 12;
const VALUE_SYMTAB_BLOCK: u64 = 14;
const METADATA_BLOCK: u64 = 15;
const TYPE_BLOCK: u64 = 17;

/// Calls `f` with every value `op` reads, in operand order.
fn for_each_operand(op: &Op, mut f: impl FnMut(V)) {
    match op {
        Op::Bin(_, a, b) | Op::Cmp(_, a, b) | Op::Store(a, b) | Op::AtomicRmw(_, a, b) => {
            f(*a);
            f(*b);
        }
        Op::Cast(_, v) | Op::Extract(v, _) | Op::Load(v) => f(*v),
        Op::Select(a, b, c) | Op::CmpXchg(a, b, c) => {
            f(*a);
            f(*b);
            f(*c);
        }
        Op::Call(_, args) => args.iter().copied().for_each(f),
        Op::Phi(inc) => inc.iter().for_each(|x| f(x.0)),
        Op::Gep(_, p, idx) => {
            f(*p);
            idx.iter().copied().for_each(f);
        }
    }
}

/// A block's successors (at most two), without allocating.
struct Succs {
    to: [usize; 2],
    len: usize,
}

impl std::ops::Deref for Succs {
    type Target = [usize];
    fn deref(&self) -> &[usize] {
        &self.to[..self.len]
    }
}

fn successors(term: Option<Term>) -> Succs {
    match term {
        Some(Term::Br(t)) => Succs { to: [t, 0], len: 1 },
        Some(Term::CondBr(_, t, f)) => Succs { to: [t, f], len: 2 },
        _ => Succs { to: [0, 0], len: 0 },
    }
}

fn chars(s: &str) -> Vec<u64> {
    s.bytes().map(u64::from).collect()
}

/// LLVM's signed VBR operand: magnitude shifted left, sign in bit 0.
fn signed(v: i64) -> u64 {
    if v >= 0 {
        (v as u64) << 1
    } else {
        (v.unsigned_abs() << 1) | 1
    }
}

/// The bitstream: little-endian 32-bit words filled from bit 0.
#[derive(Default)]
struct Writer {
    words: Vec<u32>,
    cur: u64,
    bits: u32,
    /// Per open block: abbreviation width, index of its length word.
    stack: Vec<(u32, usize)>,
}

impl Writer {
    fn width(&self) -> u32 {
        self.stack.last().map_or(2, |b| b.0)
    }

    fn emit(&mut self, v: u64, width: u32) {
        debug_assert!(width == 64 || v >> width == 0);
        self.cur |= v << self.bits;
        self.bits += width;
        while self.bits >= 32 {
            self.words.push(self.cur as u32);
            self.cur >>= 32;
            self.bits -= 32;
        }
    }

    fn vbr(&mut self, mut v: u64, width: u32) {
        let hi = 1u64 << (width - 1);
        while v >= hi {
            self.emit(v & (hi - 1) | hi, width);
            v >>= width - 1;
        }
        self.emit(v, width);
    }

    fn align(&mut self) {
        if self.bits > 0 {
            self.words.push(self.cur as u32);
            self.cur = 0;
            self.bits = 0;
        }
    }

    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.emit(u64::from(x), 8);
        }
    }

    fn enter(&mut self, id: u64, width: u32) {
        self.emit(1, self.width()); // ENTER_SUBBLOCK
        self.vbr(id, 8);
        self.vbr(u64::from(width), 4);
        self.align();
        self.words.push(0);
        self.stack.push((width, self.words.len()));
    }

    fn exit(&mut self) {
        self.emit(0, self.width()); // END_BLOCK
        self.align();
        let (_, start) = self.stack.pop().expect("open block");
        self.words[start - 1] = (self.words.len() - start) as u32;
    }

    /// An unabbreviated record.
    fn record(&mut self, code: u64, ops: &[u64]) {
        self.emit(3, self.width());
        self.vbr(code, 6);
        self.vbr(ops.len() as u64, 6);
        for &o in ops {
            self.vbr(o, 6);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::MeshVaryingSlot::{OutsideWidth, Remap, RemovedClip};
    use super::*;

    #[test]
    fn mesh_export_converts_four_half_channels_before_the_first_store() {
        use crate::emit::{container, lower_program};
        use crate::native::{MeshIo, StageIo, Usage, VsOutput};
        use crate::{Container, FourCC, HwStage, NativeProgram, io, ir, resource, structure};

        // Four different half values in v0..v3, followed by one exp param0.xyzw.
        let words: [u32; 11] = [
            0x7e00_02ff, 0x3c00, 0x7e02_02ff, 0x4000,
            0x7e04_02ff, 0x4200, 0x7e06_02ff, 0x4400,
            0xf800_0a0f, 0x0302_0100, 0xbf81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ms,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: StageIo::Ms(MeshIo {
                outputs: vec![VsOutput { usage: Usage::Generic, row: 0, param: 0, fp16: true, pack2: false }],
                clip_cntl: 0, threads: 32, max_vertices: 32, max_primitives: 1, topology: 2, lds_bytes: 0,
            }),
            code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
        };
        let output = container::SigElement {
            name: "TEXCOORD".into(), index: 0, kind: 0, comp: 9, interp: 2,
            row: Some(0), col: 0, cols: 4, usage: 15, half: true,
        };
        let signature = container::signature(&[output], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &signature)]);
        let container = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let module = lower_program(&container, &native, &p, &st, &io, &res).unwrap().shader.m;

        // Metal coalesces one element at its first scalar store. All four
        // converted values must already exist at that instruction.
        let mut stores = 0;
        for block in &module.blocks {
            let mut conversions = 0;
            for &id in &block.body {
                let Op::Call(func, _) = &module.insts[id as usize].op else { continue };
                match module.funcs[*func as usize].name.as_str() {
                    "dx.op.legacyF16ToF32" => conversions += 1,
                    "dx.op.storeVertexOutput.f32" => {
                        assert_eq!(conversions, 4, "incomplete mesh element at its store");
                        stores += 1;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(stores, 4);
    }

    #[test]
    fn repacking_channels_from_separate_exports_prepares_both_values() {
        let mut m = Module::default();
        let block = m.new_block();
        let (void, float) = (m.void(), m.float());
        let (zero, one, col, opcode) = (m.i32(0), m.i32(1), m.i8(0), m.i32(171));
        let name = "dx.op.storeVertexOutput.f32";
        let a = m.cast(block, Cast::Bitcast, zero, float);
        let first = signature_call(&mut m, block, name, void, &[opcode, zero, zero, col, a, zero]);
        let b = m.cast(block, Cast::Bitcast, one, float);
        let second = signature_call(&mut m, block, name, void, &[opcode, one, zero, col, b, zero]);
        let barrier = signature_call(&mut m, block, "dx.op.barrier", void, &[one, one]);
        let c = m.cast(block, Cast::Bitcast, opcode, float);
        let last = signature_call(&mut m, block, name, void, &[opcode, one, zero, col, c, zero]);
        let mut map = [[OutsideWidth; 4]; 2];
        map[0][0] = Remap(0, 0);
        map[1][0] = Remap(0, 1);
        m.remap_mesh_varying_calls(MeshVaryingStage::MeshOutput, &map).unwrap();
        let ordered: Vec<_> = m.blocks[block].body.iter().copied().map(V::Inst).collect();
        assert_eq!(ordered, [a, b, first, second, barrier, c, last]);
        assert_eq!(m.int_value(call_args(&m, first)[3]), Some(0));
        assert_eq!(m.int_value(call_args(&m, second)[3]), Some(1));
    }

    fn signature_call(m: &mut Module, block: usize, name: &str, ret: Ty, args: &[V]) -> V {
        let ty = m.func_ty(ret, args.iter().map(|&value| m.type_of(value)).collect());
        let func = m.function(name, ty, Some(Attr::NoUnwind), false);
        m.call(block, func, args.to_vec())
    }

    fn call_args(m: &Module, call: V) -> &[V] {
        let V::Inst(i) = call else {
            panic!("instruction expected")
        };
        let Op::Call(_, args) = &m.insts[i as usize].op else {
            panic!("call expected")
        };
        args
    }

    #[test]
    fn mesh_varying_remap_updates_both_pixel_input_forms() {
        let mut m = Module::default();
        let block = m.new_block();
        let (float, void) = (m.float(), m.void());
        let (load, at_vertex, other) = (m.i32(4), m.i32(137), m.i32(5));
        let (id, row, col, vertex, vertex8) = (m.i32(0), m.i32(0), m.i8(1), m.i32(7), m.i8(1));
        let input = signature_call(
            &mut m,
            block,
            "dx.op.loadInput.f32",
            float,
            &[load, id, row, col, vertex],
        );
        let attr = signature_call(
            &mut m,
            block,
            "dx.op.attributeAtVertex.f32",
            float,
            &[at_vertex, id, row, col, vertex8],
        );
        let unaffected = signature_call(
            &mut m,
            block,
            "dx.op.storeOutput.f32",
            void,
            &[other, id, row, col, input],
        );

        // A vec2's second component becomes the only component of its new element.
        let map = [[Remap(0, 3), Remap(1, 0), OutsideWidth, OutsideWidth]];
        m.remap_mesh_varying_calls(MeshVaryingStage::PixelInput, &map)
            .unwrap();
        let (new_id, new_col) = (m.i32(1), m.i8(0));
        assert_eq!(call_args(&m, input), &[load, new_id, row, new_col, vertex]);
        assert_eq!(
            call_args(&m, attr),
            &[at_vertex, new_id, row, new_col, vertex8]
        );
        assert_eq!(call_args(&m, unaffected), &[other, id, row, col, input]);
        assert_eq!(m.int_value(id), Some(0));
        assert_eq!(m.int_value(col), Some(1));
    }

    #[test]
    fn mesh_varying_remap_checks_all_mesh_stores_before_discarding_neutral_clip() {
        let mut m = Module::default();
        let block = m.new_block();
        let (float, void) = (m.float(), m.void());
        let (opcode, id, row, vertex) = (m.i32(171), m.i32(0), m.i32(0), m.i32(7));
        let positive = m.f32(0.0_f32.to_bits());
        let negative = m.f32((-0.0_f32).to_bits());
        let bitcast = m.cast(block, Cast::Bitcast, row, float);
        let (x, y, z, w) = (m.i8(0), m.i8(1), m.i8(2), m.i8(3));
        let name = "dx.op.storeVertexOutput.f32";
        let keep = signature_call(
            &mut m,
            block,
            name,
            void,
            &[opcode, id, row, x, negative, vertex],
        );
        let remove_const = signature_call(
            &mut m,
            block,
            name,
            void,
            &[opcode, id, row, y, positive, vertex],
        );
        let remove_bitcast = signature_call(
            &mut m,
            block,
            name,
            void,
            &[opcode, id, row, z, bitcast, vertex],
        );
        let not_neutral = signature_call(
            &mut m,
            block,
            name,
            void,
            &[opcode, id, row, w, negative, vertex],
        );
        let before = m.blocks[block].body.clone();
        let constants = m.consts.len();

        let mut map = [[Remap(2, 0), RemovedClip, RemovedClip, RemovedClip]];
        assert_eq!(
            m.remap_mesh_varying_calls(MeshVaryingStage::MeshOutput, &map)
                .unwrap_err(),
            "storeVertexOutput: clip value is a non-neutral f32 literal",
        );
        assert_eq!(m.blocks[block].body, before);
        assert_eq!(m.consts.len(), constants);
        assert_eq!(call_args(&m, keep), &[opcode, id, row, x, negative, vertex]);

        map[0][3] = Remap(2, 1);
        m.remap_mesh_varying_calls(MeshVaryingStage::MeshOutput, &map)
            .unwrap();
        let (new_id, new_col) = (m.i32(2), m.i8(1));
        assert_eq!(
            call_args(&m, keep),
            &[opcode, new_id, row, x, negative, vertex]
        );
        assert_eq!(
            call_args(&m, not_neutral),
            &[opcode, new_id, row, new_col, negative, vertex]
        );
        for removed in [remove_const, remove_bitcast] {
            let V::Inst(i) = removed else { unreachable!() };
            assert!(!m.blocks[block].body.contains(&i));
        }
    }

    #[test]
    fn mesh_varying_remap_rejects_missing_and_invalid_pixel_accesses() {
        for (row, column, replacement, reason) in [
            (0, 0, RemovedClip, "reads a removed clip component"),
            (
                0,
                0,
                OutsideWidth,
                "relative column is outside the declared element width",
            ),
            (1, 0, Remap(1, 0), "nonzero relative row"),
            (0, 4, Remap(1, 0), "relative column is outside the remap"),
            (0, 0, Remap(1, 4), "destination column is outside the remap"),
        ] {
            let mut m = Module::default();
            let block = m.new_block();
            let float = m.float();
            let args = [m.i32(4), m.i32(0), m.i32(row), m.i8(column), m.i32(0)];
            let call = signature_call(&mut m, block, "dx.op.loadInput.f32", float, &args);
            let map = [[replacement, OutsideWidth, OutsideWidth, OutsideWidth]];
            assert_eq!(
                m.remap_mesh_varying_calls(MeshVaryingStage::PixelInput, &map)
                    .unwrap_err(),
                format!("loadInput: {reason}"),
            );
            assert_eq!(call_args(&m, call), &args);
        }
    }

    #[test]
    fn mesh_varying_remap_distinguishes_a_clip_bitcast_from_an_ssa_value() {
        let mut m = Module::default();
        let entry = m.new_block();
        let block = m.new_block();
        let (float, void) = (m.float(), m.void());
        let (opcode, id, row, vertex) = (m.i32(171), m.i32(0), m.i32(0), m.i32(7));
        let col = m.i8(1);
        let bits = m.phi(block, m.type_of(row));
        m.terminate(entry, Term::Br(block));
        m.set_phi(bits, vec![(row, entry)]);
        let value = m.cast(block, Cast::Bitcast, bits, float);
        let args = [opcode, id, row, col, value, vertex];
        let call = signature_call(&mut m, block, "dx.op.storeVertexOutput.f32", void, &args);
        let before = m.blocks[block].body.clone();
        let constants = m.consts.len();
        let map = [[Remap(2, 0), RemovedClip, OutsideWidth, OutsideWidth]];

        assert_eq!(
            m.remap_mesh_varying_calls(MeshVaryingStage::MeshOutput, &map)
                .unwrap_err(),
            "storeVertexOutput: clip value is a bitcast of a phi result",
        );
        assert_eq!(m.blocks[block].body, before);
        assert_eq!(m.consts.len(), constants);
        assert_eq!(call_args(&m, call), &args);
    }

    #[test]
    fn vbr_and_blocks() {
        let mut w = Writer::default();
        w.vbr(5, 6);
        w.vbr(100, 6); // two chunks: 100 = 0b1100100 -> 0b100100 | 32, then 3
        assert_eq!(w.cur, 5 | (0b10_0100 << 6) | (3 << 12));
        let mut w = Writer::default();
        w.enter(8, 3);
        w.record(1, &[1]);
        w.exit();
        let bytes = w.finish();
        // header word, length word, one record word, end word.
        assert_eq!(bytes.len() % 4, 0);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(signed(-3), 7);
        assert_eq!(signed(2), 4);
    }

    /// A structured shader's shape: `stages` if/else diamonds in a row, each
    /// merge with one phi per register (as the lowering makes them), then a
    /// loop, and a tail that reads only the first `read` registers.
    fn synthetic(regs: usize, stages: usize, read: usize) -> Module {
        let mut m = Module::default();
        let i32t = m.int(32);
        let void = m.void();
        let fty = m.func_ty(i32t, vec![i32t, i32t]);
        let f = m.function("dx.op.synthetic", fty, Some(Attr::ReadNone), false);
        let main_ty = m.func_ty(void, vec![]);
        m.function("main", main_ty, None, true);
        let mut b = m.new_block();
        let mut cur: Vec<V> = (0..regs).map(|r| m.i32(r as u32)).collect();
        for s in 0..stages {
            let c = m.cmp(b, pred::EQ, cur[0], cur[s % regs]);
            let (t, e, j) = (m.new_block(), m.new_block(), m.new_block());
            m.terminate(b, Term::CondBr(c, t, e));
            let x = m.call(t, f, vec![cur[s % regs], cur[(s + 1) % regs]]);
            m.terminate(t, Term::Br(j));
            m.terminate(e, Term::Br(j));
            cur = (0..regs)
                .map(|r| {
                    let p = m.phi(j, i32t);
                    let tv = if r == s % regs { x } else { cur[r] };
                    m.set_phi(p, vec![(tv, t), (cur[r], e)]);
                    p
                })
                .collect();
            b = j;
        }
        // A loop: header phis fed back from an empty latch (which is folded).
        let (head, body, latch, exit) =
            (m.new_block(), m.new_block(), m.new_block(), m.new_block());
        m.terminate(b, Term::Br(head));
        let phis: Vec<V> = (0..regs).map(|_| m.phi(head, i32t)).collect();
        let one = m.i32(1);
        let stepped: Vec<V> = phis
            .iter()
            .map(|&p| m.bin(body, Bin::Add, p, one))
            .collect();
        let c = m.cmp(body, pred::IULT, stepped[0], cur[0]);
        m.terminate(head, Term::Br(body));
        m.terminate(body, Term::CondBr(c, latch, exit));
        m.terminate(latch, Term::Br(head));
        for r in 0..regs {
            m.set_phi(phis[r], vec![(cur[r], b), (stepped[r], latch)]);
        }
        let mut acc = stepped[0];
        for &v in &stepped[1..read.max(1)] {
            acc = m.bin(exit, Bin::Add, acc, v);
        }
        let _ = m.call(exit, f, vec![acc, cur[0]]);
        m.terminate(exit, Term::Ret);
        m
    }

    /// FNV-1a, to pin a written module's exact bytes.
    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// Liveness as `write` computed it before the worklist: whole passes over
    /// the instructions until one changes nothing.
    fn live_by_passes(m: &Module) -> Vec<bool> {
        let mut live = vec![false; m.insts.len()];
        for b in &m.blocks {
            for &i in &b.body {
                live[i as usize] = true;
            }
            if let Some(Term::CondBr(V::Inst(c), ..)) = b.term {
                live[c as usize] = true;
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for (i, inst) in m.insts.iter().enumerate() {
                if live[i] {
                    for_each_operand(&inst.op, |v| {
                        if let V::Inst(j) = v
                            && !live[j as usize]
                        {
                            live[j as usize] = true;
                            changed = true;
                        }
                    });
                }
            }
        }
        live
    }

    #[test]
    fn worklist_liveness_matches_passes() {
        for (regs, stages, read) in [(1, 0, 1), (1, 1, 1), (4, 3, 1), (8, 17, 3), (16, 40, 16)] {
            let m = synthetic(regs, stages, read);
            let live = m.live();
            assert_eq!(live, live_by_passes(&m), "{regs} regs x {stages} stages");
        }
    }

    #[test]
    fn unread_phis_are_dropped() {
        let mut m = Module::default();
        let i32t = m.int(32);
        let (entry, join) = (m.new_block(), m.new_block());
        let (a, b) = (m.i32(1), m.i32(2));
        m.terminate(entry, Term::Br(join));
        let read = m.phi(join, i32t);
        let unread = m.phi(join, i32t);
        // A cycle of phis that only feed each other stays dead.
        let cyc_a = m.phi(join, i32t);
        let cyc_b = m.phi(join, i32t);
        m.set_phi(read, vec![(a, entry)]);
        m.set_phi(unread, vec![(b, entry)]);
        m.set_phi(cyc_a, vec![(cyc_b, entry)]);
        m.set_phi(cyc_b, vec![(cyc_a, entry)]);
        let sum = m.bin(join, Bin::Add, read, b);
        m.terminate(join, Term::Ret);
        let ids = |v: V| match v {
            V::Inst(i) => i as usize,
            _ => unreachable!(),
        };
        let live = m.live();
        assert!(live[ids(read)] && live[ids(sum)]);
        assert!(!live[ids(unread)] && !live[ids(cyc_a)] && !live[ids(cyc_b)]);
        assert_eq!(live, live_by_passes(&m));
    }

    #[test]
    fn successors_of_each_terminator() {
        assert!(successors(None).is_empty());
        assert!(successors(Some(Term::Ret)).is_empty());
        assert_eq!(&*successors(Some(Term::Br(7))), &[7]);
        assert_eq!(&*successors(Some(Term::CondBr(V::Const(0), 3, 5))), &[3, 5]);
    }

    /// The written bytes are pinned to what the writer produced before the
    /// worklist and the reused record buffers.
    #[test]
    fn written_bytes_unchanged() {
        for ((regs, stages, read), len, hash) in [
            ((64, 64, 64), 38_916, 0x9586_730c_f953_4a45),
            ((128, 256, 32), 295_952, 0x89e5_de2d_64f0_8785),
        ] {
            let bytes = synthetic(regs, stages, read).write();
            assert_eq!((bytes.len(), fnv(&bytes)), (len, hash), "{regs}x{stages}");
        }
    }

    #[test]
    #[ignore = "benchmark: cargo test --release -p victory-shader bitcode -- --ignored --nocapture"]
    fn bench_write() {
        for (regs, stages, read) in [(64, 64, 64), (128, 256, 32), (256, 512, 256)] {
            let m = synthetic(regs, stages, read);
            let bytes = m.write();
            let runs = 5;
            let t = std::time::Instant::now();
            for _ in 0..runs {
                std::hint::black_box(m.write());
            }
            let per = t.elapsed() / runs;
            println!(
                "write {regs} regs x {stages} stages ({} insts): {per:?}, {} bytes, fnv {:#018x}",
                m.insts.len(),
                bytes.len(),
                fnv(&bytes)
            );
        }
    }
}
