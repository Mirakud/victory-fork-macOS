//! Reads the unabbreviated LLVM records produced by Victory's bitcode writer.
//! A different encoding or instruction layout is a refusal.

use super::{
    CONSTANTS_BLOCK, FUNCTION_BLOCK, METADATA_BLOCK, MODULE_BLOCK, PARAMATTR_BLOCK,
    PARAMATTR_GROUP_BLOCK, TYPE_BLOCK, VALUE_SYMTAB_BLOCK, pixel_input_opcode,
};
use crate::{DxilProgram, ShaderKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelInputType {
    F32,
    I32,
    I1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelInputRead {
    pub(crate) opcode: u32,
    pub(crate) element: u32,
    pub(crate) row: u32,
    pub(crate) column: u8,
    /// `None` is the undefined i32 vertex argument of `loadInput`.
    pub(crate) vertex: Option<u32>,
    pub(crate) component_type: PixelInputType,
}

/// Returns every reachable signature read in final emitted block order.
/// Identical reads can occur more than once and are reported as written.
pub(crate) fn pixel_input_reads(dxil_part: &[u8]) -> Result<Vec<PixelInputRead>, String> {
    let (module, body) = read_module(pixel_bitcode(dxil_part)?).map_err(str::to_owned)?;
    module.read_inputs(body).map_err(str::to_owned)
}

/// Static call-site counts in the pixel entry point's reachable blocks.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PixelBodyInventory {
    pub(crate) reachable_blocks: usize,
    /// Branch arms targeting the same or an earlier block in serialized order.
    pub(crate) backward_edges: usize,
    pub(crate) cbuffer_loads: usize,
    pub(crate) raw_buffer_loads: usize,
    pub(crate) buffer_loads: usize,
    pub(crate) samples: usize,
    pub(crate) gathers: usize,
    pub(crate) texture_loads: usize,
    pub(crate) handle_creations: usize,
    pub(crate) handle_annotations: usize,
    pub(crate) dimensions: usize,
    pub(crate) wave: usize,
    pub(crate) quad: usize,
    pub(crate) discards: usize,
    pub(crate) dxil_calls: usize,
}

pub(crate) fn pixel_body_inventory(dxil_part: &[u8]) -> Result<PixelBodyInventory, String> {
    let (module, body) = read_module(pixel_bitcode(dxil_part)?).map_err(str::to_owned)?;
    let blocks = module
        .read_body(body, |_, call, calls: &mut Vec<&str>| calls.push(call.name))
        .map_err(str::to_owned)?;
    let mut inventory = PixelBodyInventory {
        reachable_blocks: blocks.len(),
        ..Default::default()
    };
    for (index, block) in blocks {
        inventory.backward_edges += block
            .successors
            .into_iter()
            .flatten()
            .filter(|&target| target <= index)
            .count();
        for name in block.calls {
            inventory.count_call(name);
        }
    }
    Ok(inventory)
}

impl PixelBodyInventory {
    fn count_call(&mut self, name: &str) {
        self.dxil_calls += 1;
        let base = name.strip_prefix("dx.op.").unwrap_or(name);
        let base = base.split('.').next().unwrap_or(base);
        match base {
            "cbufferLoad" | "cbufferLoadLegacy" => self.cbuffer_loads += 1,
            "rawBufferLoad" => self.raw_buffer_loads += 1,
            "bufferLoad" => self.buffer_loads += 1,
            "sample" | "sampleBias" | "sampleGrad" | "sampleLevel" | "sampleCmp"
            | "sampleCmpLevelZero" | "sampleCmpLevel" | "sampleCmpBias" | "sampleCmpGrad" => {
                self.samples += 1;
            }
            "textureGather" | "textureGatherCmp" | "textureGatherRaw" => self.gathers += 1,
            "textureLoad" => self.texture_loads += 1,
            "createHandle" | "createHandleFromBinding" | "createHandleFromHeap" => {
                self.handle_creations += 1;
            }
            "annotateHandle" => self.handle_annotations += 1,
            "getDimensions" => self.dimensions += 1,
            "discard" => self.discards += 1,
            _ if base.starts_with("wave") => self.wave += 1,
            _ if base.starts_with("quad") => self.quad += 1,
            _ => {}
        }
    }
}

fn pixel_bitcode(dxil_part: &[u8]) -> Result<&[u8], String> {
    let program = DxilProgram::parse(dxil_part).map_err(|error| error.to_string())?;
    if program.kind != ShaderKind::Pixel || !program.is_standard_layout() {
        return Err("DXIL inventory requires a translated PC pixel shader".into());
    }
    Ok(program.bitcode)
}

type Parsed<T> = Result<T, &'static str>;

struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
    width: u32,
}

enum Item<'a> {
    Block(u64, BitReader<'a>),
    Record(u64),
    End,
}

impl<'a> BitReader<'a> {
    fn bits(&mut self, width: u32) -> Parsed<u64> {
        if self.bit + width as usize > self.bytes.len() * 8 {
            return Err("truncated DXIL bitstream");
        }
        let mut value = 0;
        let mut shift = 0;
        while shift < width {
            let first = self.bit % 8;
            let take = (width - shift).min((8 - first) as u32);
            let bits = u64::from(self.bytes[self.bit / 8] >> first) & ((1 << take) - 1);
            value |= bits << shift;
            self.bit += take as usize;
            shift += take;
        }
        Ok(value)
    }

    fn vbr(&mut self, width: u32) -> Parsed<u64> {
        let more = 1 << (width - 1);
        let mut value = 0;
        let mut shift = 0;
        loop {
            let chunk = self.bits(width)?;
            let low = chunk & (more - 1);
            if shift >= 64 || low > u64::MAX >> shift {
                return Err("DXIL bitstream operand overflow");
            }
            value |= low << shift;
            if chunk & more == 0 {
                return Ok(value);
            }
            shift += width - 1;
        }
    }

    fn align(&mut self) -> Parsed<()> {
        let padding = ((32 - self.bit % 32) % 32) as u32;
        if self.bits(padding)? != 0 {
            return Err("nonzero DXIL block padding");
        }
        Ok(())
    }

    fn next(&mut self, operands: &mut Vec<u64>) -> Parsed<Item<'a>> {
        match self.bits(self.width)? {
            0 => {
                self.align()?;
                if self.bit != self.bytes.len() * 8 {
                    return Err("DXIL block has trailing records");
                }
                Ok(Item::End)
            }
            1 => {
                let id = self.vbr(8)?;
                let width = self.vbr(4)?;
                if !(2..=32).contains(&width) {
                    return Err("invalid DXIL block abbreviation width");
                }
                self.align()?;
                let words = self.bits(32)? as usize;
                let start = self.bit / 8;
                let end = words
                    .checked_mul(4)
                    .and_then(|len| start.checked_add(len))
                    .ok_or("DXIL block length overflow")?;
                let bytes = self.bytes.get(start..end).ok_or("truncated DXIL block")?;
                self.bit = end * 8;
                Ok(Item::Block(
                    id,
                    Self {
                        bytes,
                        bit: 0,
                        width: width as u32,
                    },
                ))
            }
            3 => {
                let code = self.vbr(6)?;
                let count =
                    usize::try_from(self.vbr(6)?).map_err(|_| "DXIL record length overflow")?;
                if count > (self.bytes.len() * 8 - self.bit) / 6 {
                    return Err("truncated DXIL record");
                }
                operands.clear();
                for _ in 0..count {
                    operands.push(self.vbr(6)?);
                }
                Ok(Item::Record(code))
            }
            _ => Err("abbreviated DXIL records are unsupported in input inventory"),
        }
    }
}

#[derive(PartialEq, Eq)]
enum Type {
    Void,
    F32,
    Int(u64),
    Function { result: u64, parameters: Vec<u64> },
    Other,
}

enum Value {
    Global,
    Function {
        ty: u64,
        name: Option<String>,
        declaration: bool,
    },
    Constant {
        ty: u64,
        value: Constant,
    },
}

enum Constant {
    Undef,
    Integer(u64),
    Other,
}

#[derive(Default)]
struct ModuleInfo {
    types: Vec<Type>,
    values: Vec<Value>,
}

fn read_module(bitcode: &[u8]) -> Parsed<(ModuleInfo, BitReader<'_>)> {
    let mut top = BitReader {
        bytes: &bitcode[4..],
        bit: 0,
        width: 2,
    };
    let mut operands = Vec::new();
    let Item::Block(MODULE_BLOCK, mut module) = top.next(&mut operands)? else {
        return Err("DXIL input inventory expected a single LLVM module");
    };
    if top.bit != top.bytes.len() * 8 {
        return Err("DXIL input inventory expected a single LLVM module");
    }
    let mut info = ModuleInfo::default();
    let mut body = None;
    let mut relative_ids = false;
    let mut constants_read = false;
    let mut names_read = false;
    loop {
        match module.next(&mut operands)? {
            Item::End => break,
            Item::Record(1) if operands.as_slice() == [1] && !relative_ids => relative_ids = true,
            Item::Record(2 | 3) => {}
            Item::Record(7) if !constants_read && !names_read && operands.len() == 6 => {
                info.values.push(Value::Global);
            }
            Item::Record(8) if !constants_read && !names_read && operands.len() == 14 => {
                info.values.push(Value::Function {
                    ty: operands[0],
                    name: None,
                    declaration: operands[2] == 1,
                });
            }
            Item::Block(TYPE_BLOCK, block) if info.types.is_empty() => {
                info.types = read_types(block, &mut operands)?;
            }
            Item::Block(CONSTANTS_BLOCK, block) if !constants_read && !names_read => {
                info.read_constants(block, &mut operands)?;
                constants_read = true;
            }
            Item::Block(VALUE_SYMTAB_BLOCK, block) if !names_read => {
                info.read_names(block, &mut operands)?;
                names_read = true;
            }
            Item::Block(FUNCTION_BLOCK, block) if body.is_none() => body = Some(block),
            Item::Block(PARAMATTR_GROUP_BLOCK | PARAMATTR_BLOCK | METADATA_BLOCK, _) => {}
            _ => return Err("unsupported DXIL module layout in input inventory"),
        }
    }
    if !relative_ids {
        return Err("DXIL input inventory requires relative value IDs");
    }
    let mut definitions = info.values.iter().filter_map(|value| match value {
        Value::Function {
            ty,
            declaration: false,
            name,
        } => Some((*ty, name.as_deref())),
        _ => None,
    });
    let Some((ty, Some("main"))) = definitions.next() else {
        return Err("DXIL input inventory requires a main function");
    };
    if definitions.next().is_some()
        || !matches!(info.ty(ty)?, Type::Function { result, parameters }
            if parameters.is_empty() && info.ty(*result) == Ok(&Type::Void))
    {
        return Err("DXIL input inventory requires a single parameterless void function");
    }
    Ok((info, body.ok_or("missing DXIL function body")?))
}

fn read_types(mut block: BitReader<'_>, operands: &mut Vec<u64>) -> Parsed<Vec<Type>> {
    if !matches!(block.next(operands)?, Item::Record(1)) || operands.len() != 1 {
        return Err("DXIL input inventory expected a type count");
    }
    let count = operands[0];
    let mut types = Vec::new();
    loop {
        let entry = match block.next(operands)? {
            Item::End if count == types.len() as u64 => return Ok(types),
            Item::Record(2) if operands.is_empty() => Type::Void,
            Item::Record(3) if operands.is_empty() => Type::F32,
            Item::Record(7) if operands.len() == 1 => Type::Int(operands[0]),
            Item::Record(21) if operands.len() >= 2 && operands[0] == 0 => Type::Function {
                result: operands[1],
                parameters: operands[2..].to_vec(),
            },
            Item::Record(19) => continue, // Name of the next struct type.
            Item::Record(4 | 8 | 11 | 18 | 20) => Type::Other,
            _ => return Err("unsupported DXIL type record in input inventory"),
        };
        types.push(entry);
    }
}

impl ModuleInfo {
    fn ty(&self, index: u64) -> Parsed<&Type> {
        self.types
            .get(index as usize)
            .ok_or("DXIL type ID is out of range")
    }

    fn read_constants(&mut self, mut block: BitReader<'_>, operands: &mut Vec<u64>) -> Parsed<()> {
        let mut ty = None;
        loop {
            let value = match block.next(operands)? {
                Item::End => return Ok(()),
                Item::Record(1) if operands.len() == 1 => {
                    self.ty(operands[0])?;
                    ty = Some(operands[0]);
                    continue;
                }
                Item::Record(3) if operands.is_empty() => Constant::Undef,
                Item::Record(4) if operands.len() == 1 => Constant::Integer(operands[0]),
                Item::Record(6 | 7) => Constant::Other,
                _ => return Err("unsupported DXIL constant record in input inventory"),
            };
            self.values.push(Value::Constant {
                ty: ty.ok_or("DXIL constant without a type")?,
                value,
            });
        }
    }

    fn read_names(&mut self, mut block: BitReader<'_>, operands: &mut Vec<u64>) -> Parsed<()> {
        loop {
            match block.next(operands)? {
                Item::End => return Ok(()),
                Item::Record(1) if operands.len() > 1 => {
                    let value = self
                        .values
                        .get_mut(operands[0] as usize)
                        .ok_or("DXIL symbol ID is out of range")?;
                    if let Value::Function { name, .. } = value {
                        let bytes = operands[1..]
                            .iter()
                            .map(|&byte| u8::try_from(byte))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|_| "invalid DXIL symbol name")?;
                        if name.is_some() {
                            return Err("duplicate DXIL function name record");
                        }
                        *name =
                            Some(String::from_utf8(bytes).map_err(|_| "invalid DXIL symbol name")?);
                    }
                }
                _ => return Err("unsupported DXIL symbol record in input inventory"),
            }
        }
    }

    fn read_inputs(&self, body: BitReader<'_>) -> Parsed<Vec<PixelInputRead>> {
        let blocks = self.read_body(body, |next_id, call, reads: &mut Vec<_>| {
            if let Some(opcode) = pixel_input_opcode(call.name) {
                reads.push(self.read_input(next_id, opcode, call));
            } else if call.name.starts_with("dx.op.eval")
                || matches!(call.name, "dx.op.loadInput" | "dx.op.attributeAtVertex")
            {
                reads.push(Err("unsupported DXIL pixel signature read"));
            }
        })?;
        blocks
            .into_iter()
            .flat_map(|(_, block)| block.calls)
            .collect()
    }

    fn read_body<'m, T: Default>(
        &'m self,
        mut body: BitReader<'_>,
        mut collect_call: impl FnMut(usize, Call<'m, '_>, &mut T),
    ) -> Parsed<Vec<(usize, Block<T>)>> {
        let mut operands = Vec::new();
        if !matches!(body.next(&mut operands)?, Item::Record(1)) || operands.len() != 1 {
            return Err("DXIL input inventory expected a block count");
        }
        let count = usize::try_from(operands[0]).map_err(|_| "DXIL block count overflow")?;
        if count == 0 || count > body.bytes.len() {
            return Err("invalid DXIL block count");
        }
        let mut blocks = Vec::with_capacity(count);
        let mut block = Block::default();
        let mut next_id = self.values.len();
        loop {
            let code = match body.next(&mut operands)? {
                Item::Record(code) if blocks.len() < count => code,
                Item::End if blocks.len() == count => break,
                _ => return Err("unsupported DXIL function layout in input inventory"),
            };
            match (code, operands.as_slice()) {
                (10, []) | (11, [_] | [_, _, _]) => {
                    if code == 11 {
                        let target_count = if operands.len() == 1 { 1 } else { 2 };
                        for (to, &target) in
                            block.successors.iter_mut().zip(&operands[..target_count])
                        {
                            if target >= count as u64 {
                                return Err("DXIL branch target is out of range");
                            }
                            *to = Some(target as usize);
                        }
                    }
                    blocks.push(std::mem::take(&mut block));
                }
                (34, _) => {
                    let call = self.read_call(next_id, &operands)?;
                    let has_result = *call.result != Type::Void;
                    collect_call(next_id, call, &mut block.calls);
                    next_id += usize::from(has_result);
                }
                // The writer emits only these fixed-result instruction records.
                (2 | 3 | 28 | 29, [_, _, _]) | (26, [_, _]) | (16, [_, _, _, ..])
                    if code != 16 || operands.len() % 2 == 1 =>
                {
                    next_id += 1
                }
                (43, [_, _, _, ..])
                | (20, [_, _, _, _])
                | (38, [_, _, _, _, _, _])
                | (46, [_, _, _, _, _, _, _, _]) => next_id += 1,
                (44, [_, _, _, _]) => {}
                _ => return Err("unsupported DXIL instruction record in input inventory"),
            }
        }
        let mut visited = vec![false; count];
        let mut pending = vec![0];
        visited[0] = true;
        while let Some(id) = pending.pop() {
            for next in blocks[id].successors.into_iter().flatten() {
                if !visited[next] {
                    visited[next] = true;
                    pending.push(next);
                }
            }
        }
        Ok(blocks
            .into_iter()
            .enumerate()
            .filter(|(index, _)| visited[*index])
            .collect())
    }

    fn read_call<'m, 'a>(&'m self, next_id: usize, operands: &'a [u64]) -> Parsed<Call<'m, 'a>> {
        let [0, 32768, ty, function, args @ ..] = operands else {
            return Err("unsupported DXIL call record in input inventory");
        };
        let id = relative_value(next_id, *function)?;
        let Some(Value::Function {
            ty: actual,
            name: Some(name),
            declaration: true,
        }) = self.values.get(id)
        else {
            return Err("DXIL input inventory requires direct calls to declared intrinsics");
        };
        if ty != actual || !name.starts_with("dx.op.") {
            return Err("unexpected DXIL callee in input inventory");
        }
        let Type::Function { result, parameters } = self.ty(*ty)? else {
            return Err("invalid DXIL call signature");
        };
        if args.len() != parameters.len() {
            return Err("invalid DXIL call arity");
        }
        Ok(Call {
            name,
            result: self.ty(*result)?,
            args,
            parameters,
        })
    }

    fn read_input(
        &self,
        next: usize,
        opcode: u32,
        Call {
            name,
            result,
            args,
            parameters,
        }: Call<'_, '_>,
    ) -> Parsed<PixelInputRead> {
        let component_type = match (result, name.rsplit('.').next()) {
            (Type::F32, Some("f32")) => PixelInputType::F32,
            (Type::Int(32), Some("i32")) => PixelInputType::I32,
            (Type::Int(1), Some("i1")) => PixelInputType::I1,
            _ => return Err("unsupported DXIL pixel signature result type"),
        };
        let last_width = if opcode == 4 { 32 } else { 8 };
        if parameters.len() != 5
            || !parameters
                .iter()
                .zip([32, 32, 32, 8, last_width])
                .all(|(&ty, width)| self.ty(ty) == Ok(&Type::Int(width)))
        {
            return Err("invalid DXIL pixel signature operand types");
        }
        let get = |index| self.constant(next, args[index], parameters[index]);
        let number = |index, error| decoded_unsigned(get(index)?).ok_or(error);
        if number(0, "DXIL pixel signature opcode is not a constant")? != opcode {
            return Err("DXIL pixel signature opcode does not match the callee");
        }
        let element = number(
            1,
            "DXIL pixel signature element is not a nonnegative constant",
        )?;
        let row = number(2, "DXIL pixel signature row is not a nonnegative constant")?;
        let column = number(
            3,
            "DXIL pixel signature column is not a nonnegative constant",
        )?;
        if column >= 4 {
            return Err("DXIL pixel signature column exceeds four components");
        }
        let vertex = match get(4)? {
            Some(Constant::Undef) if opcode == 4 => None,
            value => {
                let index = decoded_unsigned(value)
                    .ok_or("DXIL pixel signature vertex is not representable")?;
                if opcode == 137 && index > 2 {
                    return Err("DXIL attributeAtVertex index exceeds three vertices");
                }
                Some(index)
            }
        };
        Ok(PixelInputRead {
            opcode,
            element,
            row,
            column: column as u8,
            vertex,
            component_type,
        })
    }

    fn constant(&self, next: usize, relative: u64, ty: u64) -> Parsed<Option<&Constant>> {
        match self.values.get(relative_value(next, relative)?) {
            Some(Value::Constant { ty: actual, value }) if *actual == ty => Ok(Some(value)),
            Some(Value::Constant { .. }) => {
                Err("DXIL input read operand type does not match its declaration")
            }
            _ => Ok(None),
        }
    }
}

#[derive(Default)]
struct Block<T> {
    calls: T,
    successors: [Option<usize>; 2],
}

struct Call<'m, 'a> {
    name: &'m str,
    result: &'m Type,
    args: &'a [u64],
    parameters: &'m [u64],
}

fn relative_value(next: usize, relative: u64) -> Parsed<usize> {
    let distance = usize::try_from(relative).map_err(|_| "DXIL value ID overflow")?;
    if distance == 0 {
        return Err("DXIL call cannot reference a forward value");
    }
    next.checked_sub(distance)
        .ok_or("DXIL relative value ID is out of range")
}

fn decoded_unsigned(value: Option<&Constant>) -> Option<u32> {
    match value {
        Some(Constant::Integer(encoded)) if encoded & 1 == 0 => u32::try_from(encoded >> 1).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Attr, Bin, Module, Term, Ty, V, Writer};
    use super::*;
    use crate::dxil::build_standard;

    fn module() -> Module {
        let mut m = Module::default();
        let void = m.void();
        let ty = m.func_ty(void, vec![]);
        m.function("main", ty, None, true);
        m
    }

    fn call(m: &mut Module, block: usize, name: &str, ty: Ty, args: &[V]) -> V {
        let signature = m.func_ty(ty, args.iter().map(|&arg| m.type_of(arg)).collect());
        let f = m.function(name, signature, Some(Attr::ReadNone), false);
        m.call(block, f, args.to_vec())
    }

    fn dxil(m: &Module) -> Vec<u8> {
        build_standard(0, (6, 6), 0x106, &m.write())
    }

    #[test]
    fn input_inventory_follows_block_order_and_value_ids() {
        let mut m = module();
        let entry = m.new_block();
        let a = m.new_block();
        let b = m.new_block();
        let exit = m.new_block();
        let unused = m.new_block();
        let (float, integer, boolean, void) = (m.float(), m.int(32), m.int(1), m.void());
        let (load, attribute, discard) = (m.i32(4), m.i32(137), m.i32(82));
        let (first, later, zero, row) = (m.i32(5), m.i32(7), m.i32(0), m.i32(2));
        let (x, z, vertex, undef) = (m.i8(0), m.i8(2), m.i8(2), m.undef(integer));
        let cond = call(
            &mut m,
            entry,
            "dx.op.loadInput.i1",
            boolean,
            &[load, first, zero, x, undef],
        );
        call(&mut m, entry, "dx.op.discard", void, &[discard, cond]);
        m.terminate(entry, Term::CondBr(cond, a, b));
        let number = m.bin(a, Bin::Add, zero, zero);
        let output = call(
            &mut m,
            a,
            "dx.op.attributeAtVertex.f32",
            float,
            &[attribute, later, row, z, vertex],
        );
        m.terminate(a, Term::Br(exit));
        call(
            &mut m,
            b,
            "dx.op.loadInput.i32",
            integer,
            &[load, later, row, z, zero],
        );
        m.terminate(b, Term::Br(exit));
        let phi = m.phi(exit, integer);
        m.set_phi(phi, vec![(number, a), (zero, b)]);
        let _ = m.bin(exit, Bin::Add, phi, zero);
        m.terminate(exit, Term::Ret);
        call(
            &mut m,
            unused,
            "dx.op.loadInput.f32",
            float,
            &[load, first, zero, z, undef],
        );
        m.terminate(unused, Term::Ret);
        assert_ne!(number, output);

        let reads = pixel_input_reads(&dxil(&m)).unwrap();
        assert_eq!(
            reads,
            [
                PixelInputRead {
                    opcode: 4,
                    element: 5,
                    row: 0,
                    column: 0,
                    vertex: None,
                    component_type: PixelInputType::I1
                },
                PixelInputRead {
                    opcode: 4,
                    element: 7,
                    row: 2,
                    column: 2,
                    vertex: Some(0),
                    component_type: PixelInputType::I32
                },
                PixelInputRead {
                    opcode: 137,
                    element: 7,
                    row: 2,
                    column: 2,
                    vertex: Some(2),
                    component_type: PixelInputType::F32
                },
            ]
        );
    }

    #[test]
    fn input_inventory_declines_unrepresentable_operands() {
        let mut m = module();
        let entry = m.new_block();
        let int = m.int(32);
        let (load, zero, vertex, column) = (m.i32(4), m.i32(0), m.undef(int), m.i8(0));
        let row = m.bin(entry, Bin::Add, zero, zero);
        call(
            &mut m,
            entry,
            "dx.op.loadInput.i32",
            int,
            &[load, zero, row, column, vertex],
        );
        m.terminate(entry, Term::Ret);
        assert_eq!(
            pixel_input_reads(&dxil(&m)).unwrap_err(),
            "DXIL pixel signature row is not a nonnegative constant"
        );
        assert_eq!(
            pixel_body_inventory(&dxil(&m)).unwrap(),
            PixelBodyInventory {
                reachable_blocks: 1,
                dxil_calls: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn body_inventory_counts_reachable_call_sites_and_backward_arms() {
        let mut m = module();
        let entry = m.new_block();
        let header = m.new_block();
        let latch = m.new_block();
        let exit = m.new_block();
        let unused = m.new_block();
        let int = m.int(32);
        let arg = m.i32(0);
        let condition = m.bool(true);
        // Only names and declaration/call layout matter for the inventory.
        for name in [
            "dx.op.cbufferLoadLegacy.i32",
            "dx.op.cbufferLoad.i32",
            "dx.op.rawBufferLoad.f32",
            "dx.op.bufferLoad.f32",
            "dx.op.sample.f32",
            "dx.op.sampleCmpLevelZero.f32",
            "dx.op.textureGatherCmp.f32",
            "dx.op.textureLoad.f32",
            "dx.op.createHandleFromBinding",
            "dx.op.createHandleFromHeap",
            "dx.op.annotateHandle",
            "dx.op.getDimensions",
            "dx.op.waveReadLaneFirst.i32",
            "dx.op.quadReadLaneAt.i32",
            "dx.op.sampleIndex",
        ] {
            call(&mut m, entry, name, int, &[arg]);
        }
        let unused_ty = m.func_ty(int, vec![int]);
        m.function(
            "dx.op.sampleGrad.f32",
            unused_ty,
            Some(Attr::ReadOnly),
            false,
        );
        call(&mut m, unused, "dx.op.textureLoad.f32", int, &[arg]);
        call(&mut m, latch, "dx.op.cbufferLoadLegacy.i32", int, &[arg]);
        let void = m.void();
        call(&mut m, exit, "dx.op.discard", void, &[arg, condition]);
        m.terminate(entry, Term::Br(header));
        m.terminate(header, Term::CondBr(condition, latch, exit));
        m.terminate(latch, Term::Br(header));
        m.terminate(exit, Term::Ret);
        m.terminate(unused, Term::Br(unused));

        assert_eq!(
            pixel_body_inventory(&dxil(&m)).unwrap(),
            PixelBodyInventory {
                reachable_blocks: 4,
                backward_edges: 1,
                cbuffer_loads: 3,
                raw_buffer_loads: 1,
                buffer_loads: 1,
                samples: 2,
                gathers: 1,
                texture_loads: 1,
                handle_creations: 2,
                handle_annotations: 1,
                dimensions: 1,
                wave: 1,
                quad: 1,
                discards: 1,
                dxil_calls: 17,
            }
        );
    }

    #[test]
    fn input_inventory_ignores_reads_in_serialized_unreachable_blocks() {
        let types = vec![
            Type::F32,
            Type::Int(32),
            Type::Int(8),
            Type::Function {
                result: 0,
                parameters: vec![1, 1, 1, 2, 1],
            },
        ];
        let int = |ty, n: u64| Value::Constant {
            ty,
            value: Constant::Integer(n * 2),
        };
        let info = ModuleInfo {
            types,
            values: vec![
                Value::Function {
                    ty: 3,
                    name: Some("dx.op.loadInput.f32".into()),
                    declaration: true,
                },
                int(1, 4),
                int(1, 1),
                int(1, 0),
                int(2, 0),
                Value::Constant {
                    ty: 1,
                    value: Constant::Undef,
                },
                int(2, 4),
            ],
        };
        let mut writer = Writer::default();
        writer.enter(FUNCTION_BLOCK, 4);
        writer.record(1, &[3]);
        writer.record(11, &[2]);
        // The unused block contains an invalid relative column. It still consumes ID 7.
        writer.record(34, &[0, 32768, 3, 7, 6, 5, 4, 1, 2]);
        writer.record(10, &[]);
        // Next ID is 8, so its references differ despite the first call being unreachable.
        writer.record(34, &[0, 32768, 3, 8, 7, 6, 5, 4, 3]);
        writer.record(10, &[]);
        writer.exit();
        let bytes = writer.finish();
        let mut stream = BitReader {
            bytes: &bytes,
            bit: 0,
            width: 2,
        };
        let mut operands = Vec::new();
        let Item::Block(FUNCTION_BLOCK, body) = stream.next(&mut operands).unwrap() else {
            panic!()
        };
        assert_eq!(
            info.read_inputs(body).unwrap(),
            [PixelInputRead {
                opcode: 4,
                element: 1,
                row: 0,
                column: 0,
                vertex: None,
                component_type: PixelInputType::F32
            }]
        );
    }

    #[test]
    fn input_inventory_declines_bad_streams_and_stages() {
        let mut m = module();
        let entry = m.new_block();
        m.terminate(entry, Term::Ret);
        let program = dxil(&m);
        assert!(pixel_input_reads(&program).unwrap().is_empty());
        for end in 0..program.len() {
            assert!(pixel_input_reads(&program[..end]).is_err());
            assert!(pixel_body_inventory(&program[..end]).is_err());
        }
        let compute = build_standard(5, (6, 6), 0x106, &m.write());
        assert!(pixel_input_reads(&compute).is_err());
        assert!(pixel_body_inventory(&compute).is_err());
    }
}
