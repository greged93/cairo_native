use itertools::Itertools;
use melior_next::ir::{
    operation, Block, BlockRef, Location, NamedAttribute, Region, Type, TypeLike, Value, ValueLike,
};

use crate::{compiler::Compiler, statements::Variable};
use color_eyre::Result;

impl<'ctx> Compiler<'ctx> {
    /// Creates the external function definition for printf.
    pub fn create_printf(&'ctx self) -> Result<()> {
        let region = Region::new();

        // needs to be a llvm function due to variadic args.
        let func = operation::Builder::new("llvm.func", Location::unknown(&self.context))
            .add_attributes(&NamedAttribute::new_parsed_vec(
                &self.context,
                &[
                    ("sym_name", "\"printf\""),
                    ("function_type", "!llvm.func<i32 (!llvm.ptr<i8>, ...)>"),
                    ("linkage", "#llvm.linkage<external>"),
                ],
            )?)
            .add_regions(vec![region])
            .build();

        self.module.body().append_operation(func);
        Ok(())
    }

    /// Utility function to create a printf call.
    pub fn call_printf(
        &'ctx self,
        block: BlockRef<'ctx>,
        fmt: &str,
        values: &[Value],
    ) -> Result<()> {
        let i32_type = Type::integer(&self.context, 32);
        let fmt_len = fmt.as_bytes().len();

        let i8_type = Type::integer(&self.context, 8);
        //let data_op = self.op_llvm_alloca(&block, i8_type, fmt_len)?;
        //let addr: Value = data_op.result(0)?.into();

        let arr_ty = Type::parse(
            &self.context,
            &format!("!llvm.array<{fmt_len} x {i8_type}>"),
        )
        .unwrap();

        // https://discourse.llvm.org/t/array-globals-in-llvm-dialect/68229
        // To create a constant array, we need to use dense, as a tensor type, which is then interpreted as a llvm.array type.
        let fmt_data = self.op_llvm_const(
            &block,
            &format!(
                "dense<[{}]> : tensor<{} x {}>",
                fmt.as_bytes().iter().join(", "),
                fmt_len,
                i8_type
            ),
            arr_ty,
        );

        // self.op_llvm_store(&block, fmt_data.result(0)?.into(), addr)?;

        let mut args = vec![fmt_data.result(0)?.into()];
        args.extend(values);

        self.op_llvm_call(&block, "printf", &args, &[i32_type])?;
        Ok(())
    }

    /// creates te implementation for the print felt method: "print_felt(value: i256) -> ()"
    pub fn create_print_felt(&'ctx self) -> Result<()> {
        let region = Region::new();

        let args_types = [(self.felt_type(), Location::unknown(&self.context))];

        let block = Block::new(&args_types);
        let block = region.append_block(block);

        let mut current_value = Variable::param(0, block);
        let mut value_type = self.felt_type();

        let mut bit_width = current_value.get_value().r#type().get_width().unwrap();

        // We need to make sure the bit width is a power of 2.
        let rounded_up_bitwidth = round_up(bit_width);

        if bit_width != rounded_up_bitwidth {
            value_type = Type::integer(&self.context, rounded_up_bitwidth);
            let res = self.op_zext(&block, current_value.get_value(), value_type);
            current_value = Variable::local(res, 0, block);
        }

        bit_width = rounded_up_bitwidth;

        while bit_width > 0 {
            let shift_by_constant_op = self.op_const(
                &block,
                &bit_width.saturating_sub(32).to_string(),
                value_type,
            );
            let shift_by = shift_by_constant_op.result(0)?.into();

            let shift_op = self.op_shrs(&block, current_value.get_value(), shift_by);
            let shift_result = shift_op.result(0)?.into();

            let truncated_op = self.op_trunc(&block, shift_result, self.i32_type());
            let truncated = truncated_op.result(0)?.into();
            self.call_printf(block, "%08X\0", &[truncated])?;

            bit_width = bit_width.saturating_sub(32);
        }

        self.op_return(&block, &[]);

        let function_type = self.create_fn_signature(&args_types, &[]);

        let func = self.op_func("print_felt", &function_type, vec![region], false, false)?;

        self.module.body().append_operation(func);

        Ok(())
    }

    /// Utility method to create a print_felt call.
    pub fn call_print_felt(&'ctx self, block: BlockRef<'ctx>, value: Value) -> Result<()> {
        self.op_func_call(&block, "print_felt", &[value], &[])?;
        Ok(())
    }
}

/// rounds to the nearest power of 2 up.
#[inline]
const fn round_up(value: u32) -> u32 {
    let mut power = 1;
    while power < value {
        power *= 2;
    }
    power
}
