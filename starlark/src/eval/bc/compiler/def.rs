/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Compile def.

use gazebo::prelude::*;

use crate::eval::bc::instr_impl::InstrDef;
use crate::eval::bc::instr_impl::InstrDefData;
use crate::eval::bc::stack_ptr::BcSlotOut;
use crate::eval::bc::writer::BcWriter;
use crate::eval::compiler::def::DefCompiled;
use crate::eval::compiler::def::ParametersCompiled;
use crate::eval::compiler::span::IrSpanned;
use crate::eval::runtime::frame_span::FrameSpan;

impl DefCompiled {
    pub(crate) fn mark_definitely_assigned_after(&self, bc: &mut BcWriter) {
        // TODO(nga): argument default values and types can be used
        //   to mark variables definitely assigned.
        let _ = bc;
    }

    pub(crate) fn write_bc(&self, span: FrameSpan, target: BcSlotOut, bc: &mut BcWriter) {
        let DefCompiled {
            ref function_name,
            ref params,
            ref return_type,
            info,
        } = *self;
        let function_name = function_name.clone();

        let how_many_slots_we_need = params.count_exprs() + return_type.as_ref().map_or(0, |_| 1);

        bc.alloc_slots(how_many_slots_we_need, |slots, bc| {
            let mut slots_i = slots.iter();
            let mut value_count = 0;
            let params = params.params.map(|p| {
                p.map(|p| {
                    p.map_expr(|e| {
                        e.write_bc(slots_i.next().unwrap().to_out(), bc);
                        value_count += 1;
                        value_count - 1
                    })
                })
            });
            let return_type = return_type.as_ref().map(|t| {
                t.write_bc(slots_i.next().unwrap().to_out(), bc);
                value_count += 1;
                IrSpanned {
                    node: value_count - 1,
                    span: t.span,
                }
            });

            let params = ParametersCompiled { params };
            let instr_def_data = InstrDefData {
                function_name,
                params,
                return_type,
                info,
            };

            assert!(slots_i.next().is_none());

            bc.write_instr::<InstrDef>(span, (slots.to_in(), instr_def_data, target));
        })
    }
}
