pub use self::{aot::AotNativeExecutor, jit::JitNativeExecutor};
use crate::{
    execution_result::ExecutionResult, types::TypeBuilder, utils::get_integer_layout,
    values::JitValue,
};
use bumpalo::Bump;
use cairo_lang_sierra::{
    extensions::{
        core::{CoreLibfunc, CoreType, CoreTypeConcrete},
        starknet::StarkNetTypeConcrete,
    },
    ids::ConcreteTypeId,
    program::FunctionSignature,
    program_registry::{ProgramRegistry, ProgramRegistryError},
};
use libc::c_void;
use std::{
    alloc::Layout,
    arch::global_asm,
    ptr::{null_mut, NonNull},
    rc::Rc,
};

mod aot;
mod jit;

#[cfg(target_arch = "aarch64")]
global_asm!(include_str!("arch/aarch64.s"));
#[cfg(target_arch = "x86_64")]
global_asm!(include_str!("arch/x86_64.s"));

extern "C" {
    /// Invoke an AOT-compiled function.
    ///
    /// The `ret_ptr` argument is only used when the first argument (the actual return pointer) is
    /// unused. Used for u8, u16, u32, u64, u128 and felt252, but not for arrays, enums or structs.
    fn aot_trampoline(
        fn_ptr: *const c_void,
        args_ptr: *const u64,
        args_len: usize,
        ret_ptr: *mut u64,
    );
}

pub enum NativeExecutor<'m> {
    Aot(Rc<AotNativeExecutor>),
    Jit(Rc<JitNativeExecutor<'m>>),
}

impl<'m> From<AotNativeExecutor> for NativeExecutor<'m> {
    fn from(value: AotNativeExecutor) -> Self {
        Self::Aot(Rc::new(value))
    }
}

impl<'m> From<JitNativeExecutor<'m>> for NativeExecutor<'m> {
    fn from(value: JitNativeExecutor<'m>) -> Self {
        Self::Jit(Rc::new(value))
    }
}

fn invoke_dynamic(
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    function_ptr: *const c_void,
    function_signature: &FunctionSignature,
    args: &[JitValue],
    gas: Option<u128>,
    syscall_handler: Option<NonNull<()>>,
) -> ExecutionResult {
    tracing::info!("Invoking function with signature: {function_signature:?}.");

    let arena = Bump::new();
    let mut invoke_data = ArgumentMapper::new(&arena, registry);

    // Generate return pointer (if necessary).
    //
    // Generated when either:
    //   - There are more than one non-zst return values.
    //     - All builtins except GasBuiltin and Starknet are ZST.
    //     - The unit struct is a ZST.
    //   - The return argument is complex.
    let mut ret_types_iter = function_signature
        .ret_types
        .iter()
        .filter(|id| {
            let info = registry.get_type(id).unwrap();

            let is_builtin = <CoreTypeConcrete as TypeBuilder<CoreType, CoreLibfunc>>::is_builtin;
            let is_zst = <CoreTypeConcrete as TypeBuilder<CoreType, CoreLibfunc>>::is_zst;

            !(is_builtin(info) && is_zst(info, registry))
        })
        .peekable();

    let num_return_args = ret_types_iter.clone().count();
    let mut return_ptr = if num_return_args > 1
        || ret_types_iter
            .peek()
            .is_some_and(|id| registry.get_type(id).unwrap().is_complex(registry))
    {
        let layout = ret_types_iter.fold(Layout::new::<()>(), |layout, id| {
            let type_info = registry.get_type(id).unwrap();
            layout
                .extend(type_info.layout(registry).unwrap())
                .unwrap()
                .0
        });

        let return_ptr = arena.alloc_layout(layout).cast::<()>();
        invoke_data.push_aligned(
            get_integer_layout(64).align(),
            &[return_ptr.as_ptr() as u64],
        );

        Some(return_ptr)
    } else {
        None
    };

    // Generate argument list.
    let mut iter = args.iter();
    for type_id in function_signature.param_types.iter().filter(|id| {
        let info = registry.get_type(id).unwrap();
        !<CoreTypeConcrete as TypeBuilder<CoreType, CoreLibfunc>>::is_zst(info, registry)
    }) {
        let type_info = registry.get_type(type_id).unwrap();

        // Process gas requirements and syscall handler.
        match type_info {
            CoreTypeConcrete::GasBuiltin(_) => match gas {
                Some(gas) => {
                    invoke_data.push_aligned(
                        get_integer_layout(128).align(),
                        &[gas as u64, (gas >> 64) as u64],
                    );
                }
                None => panic!("Gas is required"),
            },
            CoreTypeConcrete::StarkNet(StarkNetTypeConcrete::System(_)) => match syscall_handler {
                Some(syscall_handler) => invoke_data.push_aligned(
                    get_integer_layout(64).align(),
                    &[syscall_handler.as_ptr() as u64],
                ),
                None => panic!("Syscall handler is required"),
            },
            _ => invoke_data
                .push(type_id, type_info, iter.next().unwrap())
                .unwrap(),
        }
    }

    // Invoke the trampoline.
    #[cfg(target_arch = "x86_64")]
    let mut ret_registers = [0; 2];
    #[cfg(target_arch = "aarch64")]
    let mut ret_registers = [0; 4];

    unsafe {
        aot_trampoline(
            function_ptr,
            invoke_data.invoke_data().as_ptr(),
            invoke_data.invoke_data().len(),
            ret_registers.as_mut_ptr(),
        );
    }

    // Parse final gas.
    let mut remaining_gas = None;
    for type_id in &function_signature.ret_types {
        let type_info = registry.get_type(type_id).unwrap();
        match type_info {
            CoreTypeConcrete::GasBuiltin(_) => {
                remaining_gas = Some(match &mut return_ptr {
                    Some(return_ptr) => unsafe {
                        let ptr = return_ptr.cast::<u128>();
                        *return_ptr = NonNull::new_unchecked(ptr.as_ptr().add(1)).cast();
                        *ptr.as_ref()
                    },
                    None => {
                        // If there's no return ptr then the function only returned the gas. We don't
                        // need to bother with the syscall handler builtin.
                        ((ret_registers[1] as u128) << 64) | ret_registers[0] as u128
                    }
                });
            }
            CoreTypeConcrete::StarkNet(StarkNetTypeConcrete::System(_)) => match &mut return_ptr {
                Some(return_ptr) => unsafe {
                    let ptr = return_ptr.cast::<*mut ()>();
                    *return_ptr = NonNull::new_unchecked(ptr.as_ptr().add(1)).cast();
                },
                None => {}
            },
            _ if <CoreTypeConcrete as TypeBuilder<CoreType, CoreLibfunc>>::is_builtin(
                type_info,
            ) => {}
            _ => break,
        }
    }

    // Parse return values.
    let return_value = parse_result(
        function_signature.ret_types.last().unwrap(),
        registry,
        return_ptr,
        ret_registers,
    );

    // FIXME: Arena deallocation.
    std::mem::forget(arena);

    ExecutionResult {
        remaining_gas,
        return_value,
    }
}

pub struct ArgumentMapper<'a> {
    arena: &'a Bump,
    registry: &'a ProgramRegistry<CoreType, CoreLibfunc>,

    invoke_data: Vec<u64>,
}

impl<'a> ArgumentMapper<'a> {
    pub fn new(arena: &'a Bump, registry: &'a ProgramRegistry<CoreType, CoreLibfunc>) -> Self {
        Self {
            arena,
            registry,
            invoke_data: Vec::new(),
        }
    }

    pub fn invoke_data(&self) -> &[u64] {
        &self.invoke_data
    }

    #[cfg_attr(target_arch = "x86_64", allow(unused_mut))]
    pub fn push_aligned(&mut self, align: usize, mut values: &[u64]) {
        assert!(align.is_power_of_two());
        assert!(align <= 16);

        // x86_64's max alignment is 8 bytes.
        #[cfg(target_arch = "x86_64")]
        assert!(align < 16);

        #[cfg(target_arch = "aarch64")]
        if align == 16 {
            // This works because on both aarch64 and x86_64 the stack is already aligned to
            // 16 bytes when the trampoline starts pushing values.
            if self.invoke_data.len() >= 8 {
                if self.invoke_data.len() & 1 != 0 {
                    self.invoke_data.push(0);
                }
            } else if self.invoke_data.len() + 1 >= 8 {
                self.invoke_data.push(0);
            } else if self.invoke_data.len() + values.len() >= 8 {
                let chunk;
                (chunk, values) = values.split_at(4);
                self.invoke_data.extend(chunk);
                self.invoke_data.push(0);
            }
        }

        self.invoke_data.extend(values);
    }

    pub fn push(
        &mut self,
        type_id: &ConcreteTypeId,
        type_info: &CoreTypeConcrete,
        value: &JitValue,
    ) -> Result<(), Box<ProgramRegistryError>> {
        match (type_info, value) {
            (CoreTypeConcrete::Array(info), JitValue::Array(values)) => {
                // TODO: Assert that `info.ty` matches all the values' types.

                let type_info = self.registry.get_type(&info.ty)?;
                let type_layout = type_info.layout(self.registry).unwrap().pad_to_align();

                // This needs to be a heap-allocated pointer because it's the actual array data.
                let ptr = if values.is_empty() {
                    null_mut()
                } else {
                    unsafe { libc::realloc(null_mut(), type_layout.size() * values.len()) }
                };

                for (idx, value) in values.iter().enumerate() {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            value
                                .to_jit(self.arena, self.registry, &info.ty)
                                .unwrap()
                                .cast()
                                .as_ptr(),
                            (ptr as usize + type_layout.size() * idx) as *mut u8,
                            type_layout.size(),
                        );
                    }
                }

                self.push_aligned(
                    get_integer_layout(64).align(),
                    &[ptr as u64, values.len() as u64, values.len() as u64],
                );
            }
            (CoreTypeConcrete::EcPoint(_), JitValue::EcPoint(a, b)) => {
                self.push_aligned(get_integer_layout(252).align(), &a.to_le_digits());
                self.push_aligned(get_integer_layout(252).align(), &b.to_le_digits());
            }
            (CoreTypeConcrete::EcState(_), JitValue::EcState(a, b, c, d)) => {
                self.push_aligned(get_integer_layout(252).align(), &a.to_le_digits());
                self.push_aligned(get_integer_layout(252).align(), &b.to_le_digits());
                self.push_aligned(get_integer_layout(252).align(), &c.to_le_digits());
                self.push_aligned(get_integer_layout(252).align(), &d.to_le_digits());
            }
            (CoreTypeConcrete::Enum(info), JitValue::Enum { tag, value, .. }) => {
                if type_info.is_memory_allocated(self.registry) {
                    let (layout, tag_layout, variant_layouts) =
                        crate::types::r#enum::get_layout_for_variants(
                            self.registry,
                            &info.variants,
                        )
                        .unwrap();

                    let ptr = self.arena.alloc_layout(layout);
                    unsafe {
                        match tag_layout.size() {
                            0 => {}
                            1 => *ptr.cast::<u8>().as_mut() = *tag as u8,
                            2 => *ptr.cast::<u16>().as_mut() = *tag as u16,
                            4 => *ptr.cast::<u32>().as_mut() = *tag as u32,
                            8 => *ptr.cast::<u64>().as_mut() = *tag as u64,
                            _ => unreachable!(),
                        }
                    }

                    let offset = tag_layout.extend(variant_layouts[*tag]).unwrap().1;
                    let payload_ptr = value
                        .to_jit(self.arena, self.registry, &info.variants[*tag])
                        .unwrap();
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            payload_ptr.cast::<u8>().as_ptr(),
                            ptr.cast::<u8>().as_ptr().add(offset),
                            variant_layouts[*tag].size(),
                        );
                    }

                    self.invoke_data.push(ptr.as_ptr() as u64);
                } else {
                    // Write the tag.
                    match (info.variants.len().next_power_of_two().trailing_zeros() + 7) / 8 {
                        0 => {}
                        _ => self.invoke_data.push(*tag as u64),
                    }

                    // Write the payload.
                    let type_info = self.registry.get_type(&info.variants[*tag]).unwrap();
                    self.push(&info.variants[*tag], type_info, value)?;
                }
            }
            (
                CoreTypeConcrete::Felt252(_)
                | CoreTypeConcrete::StarkNet(
                    StarkNetTypeConcrete::ClassHash(_)
                    | StarkNetTypeConcrete::ContractAddress(_)
                    | StarkNetTypeConcrete::StorageAddress(_)
                    | StarkNetTypeConcrete::StorageBaseAddress(_),
                ),
                JitValue::Felt252(value),
            ) => {
                self.push_aligned(get_integer_layout(252).align(), &value.to_le_digits());
            }
            (CoreTypeConcrete::Felt252Dict(_), JitValue::Felt252Dict { .. }) => {
                #[cfg(not(feature = "cairo-native-runtime"))]
                unimplemented!("enable the `cairo-native-runtime` feature to use felt252 dicts");

                // TODO: Assert that `info.ty` matches all the values' types.

                self.invoke_data.push(
                    value
                        .to_jit(self.arena, self.registry, type_id)
                        .unwrap()
                        .as_ptr() as u64,
                );
            }
            (CoreTypeConcrete::Struct(info), JitValue::Struct { fields, .. }) => {
                for (field_type_id, field_value) in info.members.iter().zip(fields) {
                    self.push(
                        field_type_id,
                        self.registry.get_type(field_type_id)?,
                        field_value,
                    )?;
                }
            }
            (CoreTypeConcrete::Uint128(_), JitValue::Uint128(value)) => self.push_aligned(
                get_integer_layout(128).align(),
                &[*value as u64, (value >> 64) as u64],
            ),
            (CoreTypeConcrete::Uint64(_), JitValue::Uint64(value)) => {
                self.push_aligned(get_integer_layout(64).align(), &[*value]);
            }
            (CoreTypeConcrete::Uint32(_), JitValue::Uint32(value)) => {
                self.push_aligned(get_integer_layout(32).align(), &[*value as u64]);
            }
            (CoreTypeConcrete::Uint16(_), JitValue::Uint16(value)) => {
                self.push_aligned(get_integer_layout(16).align(), &[*value as u64]);
            }
            (CoreTypeConcrete::Uint8(_), JitValue::Uint8(value)) => {
                self.push_aligned(get_integer_layout(8).align(), &[*value as u64]);
            }
            (CoreTypeConcrete::Sint128(_), JitValue::Sint128(value)) => {
                self.push_aligned(
                    get_integer_layout(128).align(),
                    &[*value as u64, (value >> 64) as u64],
                );
            }
            (CoreTypeConcrete::Sint64(_), JitValue::Sint64(value)) => {
                self.push_aligned(get_integer_layout(64).align(), &[*value as u64]);
            }
            (CoreTypeConcrete::Sint32(_), JitValue::Sint32(value)) => {
                self.push_aligned(get_integer_layout(32).align(), &[*value as u64]);
            }
            (CoreTypeConcrete::Sint16(_), JitValue::Sint16(value)) => {
                self.push_aligned(get_integer_layout(16).align(), &[*value as u64]);
            }
            (CoreTypeConcrete::Sint8(_), JitValue::Sint8(value)) => {
                self.push_aligned(get_integer_layout(8).align(), &[*value as u64]);
            }
            (CoreTypeConcrete::NonZero(info), _) => {
                // TODO: Check that the value is indeed non-zero.
                let type_info = self.registry.get_type(&info.ty)?;
                self.push(&info.ty, type_info, value)?;
            }
            (CoreTypeConcrete::Snapshot(info), _) => {
                let type_info = self.registry.get_type(&info.ty)?;
                self.push(&info.ty, type_info, value)?;
            }
            (CoreTypeConcrete::Bitwise(_), _)
            | (CoreTypeConcrete::BuiltinCosts(_), _)
            | (CoreTypeConcrete::EcOp(_), _)
            | (CoreTypeConcrete::Pedersen(_), _)
            | (CoreTypeConcrete::Poseidon(_), _)
            | (CoreTypeConcrete::RangeCheck(_), _)
            | (CoreTypeConcrete::SegmentArena(_), _) => {}
            (sierra_ty, arg_ty) => match sierra_ty {
                CoreTypeConcrete::Array(_) => todo!("Array {arg_ty:?}"),
                CoreTypeConcrete::Bitwise(_) => todo!("Bitwise {arg_ty:?}"),
                CoreTypeConcrete::Box(_) => todo!("Box {arg_ty:?}"),
                CoreTypeConcrete::EcOp(_) => todo!("EcOp {arg_ty:?}"),
                CoreTypeConcrete::EcPoint(_) => todo!("EcPoint {arg_ty:?}"),
                CoreTypeConcrete::EcState(_) => todo!("EcState {arg_ty:?}"),
                CoreTypeConcrete::Felt252(_) => todo!("Felt252 {arg_ty:?}"),
                CoreTypeConcrete::GasBuiltin(_) => todo!("GasBuiltin {arg_ty:?}"),
                CoreTypeConcrete::BuiltinCosts(_) => todo!("BuiltinCosts {arg_ty:?}"),
                CoreTypeConcrete::Uint8(_) => todo!("Uint8 {arg_ty:?}"),
                CoreTypeConcrete::Uint16(_) => todo!("Uint16 {arg_ty:?}"),
                CoreTypeConcrete::Uint32(_) => todo!("Uint32 {arg_ty:?}"),
                CoreTypeConcrete::Uint64(_) => todo!("Uint64 {arg_ty:?}"),
                CoreTypeConcrete::Uint128(_) => todo!("Uint128 {arg_ty:?}"),
                CoreTypeConcrete::Uint128MulGuarantee(_) => todo!("Uint128MulGuarantee {arg_ty:?}"),
                CoreTypeConcrete::Sint8(_) => todo!("Sint8 {arg_ty:?}"),
                CoreTypeConcrete::Sint16(_) => todo!("Sint16 {arg_ty:?}"),
                CoreTypeConcrete::Sint32(_) => todo!("Sint32 {arg_ty:?}"),
                CoreTypeConcrete::Sint64(_) => todo!("Sint64 {arg_ty:?}"),
                CoreTypeConcrete::Sint128(_) => todo!("Sint128 {arg_ty:?}"),
                CoreTypeConcrete::NonZero(_) => todo!("NonZero {arg_ty:?}"),
                CoreTypeConcrete::Nullable(_) => todo!("Nullable {arg_ty:?}"),
                CoreTypeConcrete::RangeCheck(_) => todo!("RangeCheck {arg_ty:?}"),
                CoreTypeConcrete::Uninitialized(_) => todo!("Uninitialized {arg_ty:?}"),
                CoreTypeConcrete::Enum(_) => todo!("Enum {arg_ty:?}"),
                CoreTypeConcrete::Struct(_) => todo!("Struct {arg_ty:?}"),
                CoreTypeConcrete::Felt252Dict(_) => todo!("Felt252Dict {arg_ty:?}"),
                CoreTypeConcrete::Felt252DictEntry(_) => todo!("Felt252DictEntry {arg_ty:?}"),
                CoreTypeConcrete::SquashedFelt252Dict(_) => todo!("SquashedFelt252Dict {arg_ty:?}"),
                CoreTypeConcrete::Pedersen(_) => todo!("Pedersen {arg_ty:?}"),
                CoreTypeConcrete::Poseidon(_) => todo!("Poseidon {arg_ty:?}"),
                CoreTypeConcrete::Span(_) => todo!("Span {arg_ty:?}"),
                CoreTypeConcrete::StarkNet(_) => todo!("StarkNet {arg_ty:?}"),
                CoreTypeConcrete::SegmentArena(_) => todo!("SegmentArena {arg_ty:?}"),
                CoreTypeConcrete::Snapshot(_) => todo!("Snapshot {arg_ty:?}"),
                CoreTypeConcrete::Bytes31(_) => todo!("Bytes31 {arg_ty:?}"),
            },
        }

        Ok(())
    }
}

fn parse_result(
    type_id: &ConcreteTypeId,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    return_ptr: Option<NonNull<()>>,
    #[cfg(target_arch = "x86_64")] ret_registers: [u64; 2],
    #[cfg(target_arch = "aarch64")] ret_registers: [u64; 4],
) -> JitValue {
    let type_info = registry.get_type(type_id).unwrap();

    match type_info {
        CoreTypeConcrete::Array(_) => JitValue::from_jit(return_ptr.unwrap(), type_id, registry),
        CoreTypeConcrete::Box(info) => unsafe {
            let ptr = return_ptr.unwrap_or(NonNull::new_unchecked(ret_registers[0] as *mut ()));
            let value = JitValue::from_jit(ptr, &info.ty, registry);
            libc::free(ptr.cast().as_ptr());
            value
        },
        CoreTypeConcrete::EcPoint(_) => JitValue::from_jit(return_ptr.unwrap(), type_id, registry),
        CoreTypeConcrete::EcState(_) => JitValue::from_jit(return_ptr.unwrap(), type_id, registry),
        CoreTypeConcrete::Felt252(_)
        | CoreTypeConcrete::StarkNet(
            StarkNetTypeConcrete::ClassHash(_)
            | StarkNetTypeConcrete::ContractAddress(_)
            | StarkNetTypeConcrete::StorageAddress(_)
            | StarkNetTypeConcrete::StorageBaseAddress(_),
        ) => {
            #[cfg(target_arch = "x86_64")]
            let value = JitValue::from_jit(return_ptr.unwrap(), type_id, registry);

            #[cfg(target_arch = "aarch64")]
            let value = JitValue::Felt252(starknet_types_core::felt::Felt::from_bytes_le(unsafe {
                std::mem::transmute::<&[u64; 4], &[u8; 32]>(&ret_registers)
            }));

            value
        }
        CoreTypeConcrete::Uint8(_) => match return_ptr {
            Some(return_ptr) => JitValue::Uint8(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Uint8(ret_registers[0] as u8),
        },
        CoreTypeConcrete::Uint16(_) => match return_ptr {
            Some(return_ptr) => JitValue::Uint16(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Uint16(ret_registers[0] as u16),
        },
        CoreTypeConcrete::Uint32(_) => match return_ptr {
            Some(return_ptr) => JitValue::Uint32(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Uint32(ret_registers[0] as u32),
        },
        CoreTypeConcrete::Uint64(_) => match return_ptr {
            Some(return_ptr) => JitValue::Uint64(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Uint64(ret_registers[0]),
        },
        CoreTypeConcrete::Uint128(_) => match return_ptr {
            Some(return_ptr) => JitValue::Uint128(unsafe { *return_ptr.cast().as_ref() }),
            None => {
                JitValue::Uint128(((ret_registers[1] as u128) << 64) | ret_registers[0] as u128)
            }
        },
        CoreTypeConcrete::Uint128MulGuarantee(_) => todo!(),
        CoreTypeConcrete::Sint8(_) => match return_ptr {
            Some(return_ptr) => JitValue::Sint8(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Sint8(ret_registers[0] as i8),
        },
        CoreTypeConcrete::Sint16(_) => match return_ptr {
            Some(return_ptr) => JitValue::Sint16(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Sint16(ret_registers[0] as i16),
        },
        CoreTypeConcrete::Sint32(_) => match return_ptr {
            Some(return_ptr) => JitValue::Sint32(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Sint32(ret_registers[0] as i32),
        },
        CoreTypeConcrete::Sint64(_) => match return_ptr {
            Some(return_ptr) => JitValue::Uint64(unsafe { *return_ptr.cast().as_ref() }),
            None => JitValue::Sint64(ret_registers[0] as i64),
        },
        CoreTypeConcrete::Sint128(_) => match return_ptr {
            Some(return_ptr) => JitValue::Uint128(unsafe { *return_ptr.cast().as_ref() }),
            None => {
                JitValue::Sint128(((ret_registers[1] as i128) << 64) | ret_registers[0] as i128)
            }
        },
        CoreTypeConcrete::NonZero(info) => {
            parse_result(&info.ty, registry, return_ptr, ret_registers)
        }
        CoreTypeConcrete::Nullable(info) => unsafe {
            let ptr = return_ptr.map_or(ret_registers[0] as *mut (), |x| {
                *x.cast::<*mut ()>().as_ref()
            });
            if ptr.is_null() {
                JitValue::Null
            } else {
                let ptr = NonNull::new_unchecked(ptr);
                let value = JitValue::from_jit(ptr, &info.ty, registry);
                libc::free(ptr.as_ptr().cast());
                value
            }
        },
        CoreTypeConcrete::Uninitialized(_) => todo!(),
        CoreTypeConcrete::Enum(info) => {
            let (_, tag_layout, variant_layouts) =
                crate::types::r#enum::get_layout_for_variants(registry, &info.variants).unwrap();

            let (tag, ptr) = if type_info.is_memory_allocated(registry) {
                let ptr = return_ptr.unwrap();

                let tag = unsafe {
                    match tag_layout.size() {
                        0 => 0,
                        1 => *ptr.cast::<u8>().as_ref() as usize,
                        2 => *ptr.cast::<u16>().as_ref() as usize,
                        4 => *ptr.cast::<u32>().as_ref() as usize,
                        8 => *ptr.cast::<u64>().as_ref() as usize,
                        _ => unreachable!(),
                    }
                };

                (tag, unsafe {
                    NonNull::new_unchecked(
                        ptr.cast::<u8>()
                            .as_ptr()
                            .add(tag_layout.extend(variant_layouts[tag]).unwrap().1),
                    )
                    .cast()
                })
            } else {
                // TODO: Shouldn't the pointer be always `None` within this block?
                match (info.variants.len().next_power_of_two().trailing_zeros() + 7) / 8 {
                    0 => (0, return_ptr.unwrap_or_else(NonNull::dangling)),
                    _ => (
                        match tag_layout.size() {
                            0 => 0,
                            1 => ret_registers[0] as u8 as usize,
                            2 => ret_registers[0] as u16 as usize,
                            4 => ret_registers[0] as u32 as usize,
                            8 => ret_registers[0] as usize,
                            _ => unreachable!(),
                        },
                        return_ptr.unwrap_or_else(NonNull::dangling),
                    ),
                }
            };
            let value = Box::new(JitValue::from_jit(ptr, &info.variants[tag], registry));

            JitValue::Enum {
                tag,
                value,
                debug_name: type_id.debug_name.as_deref().map(ToString::to_string),
            }
        }
        CoreTypeConcrete::Struct(info) => {
            if info.members.is_empty() {
                JitValue::Struct {
                    fields: Vec::new(),
                    debug_name: type_id.debug_name.as_deref().map(ToString::to_string),
                }
            } else {
                JitValue::from_jit(return_ptr.unwrap(), type_id, registry)
            }
        }
        CoreTypeConcrete::Felt252Dict(_) => match return_ptr {
            Some(return_ptr) => JitValue::from_jit(return_ptr, type_id, registry),
            None => JitValue::from_jit(
                NonNull::new(ret_registers[0] as *mut ()).unwrap(),
                type_id,
                registry,
            ),
        },
        CoreTypeConcrete::Felt252DictEntry(_) => todo!(),
        CoreTypeConcrete::SquashedFelt252Dict(_) => todo!(),
        CoreTypeConcrete::Span(_) => todo!(),
        CoreTypeConcrete::Snapshot(_) => todo!(),
        CoreTypeConcrete::Bytes31(_) => todo!(),
        _ => unreachable!(),
    }
}
