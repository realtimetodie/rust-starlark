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

use std::any::TypeId;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::fmt::Display;
use std::marker::PhantomData;
use std::mem;
use std::ptr;

use allocative::Allocative;
use dupe::Dupe;

use crate::any::AnyLifetime;
use crate::any::ProvidesStaticType;
use crate::collections::Hashed;
use crate::collections::StarlarkHashValue;
use crate::collections::StarlarkHasher;
use crate::docs::DocItem;
use crate::environment::Methods;
use crate::eval::Arguments;
use crate::eval::Evaluator;
use crate::private::Private;
use crate::values::demand::Demand;
use crate::values::layout::avalue::AValue;
use crate::values::layout::avalue::BlackHole;
use crate::values::layout::const_type_id::ConstTypeId;
use crate::values::layout::heap::repr::AValueHeader;
use crate::values::layout::heap::repr::AValueRepr;
use crate::values::traits::StarlarkValueVTable;
use crate::values::traits::StarlarkValueVTableGet;
use crate::values::Freezer;
use crate::values::FrozenStringValue;
use crate::values::FrozenValue;
use crate::values::Heap;
use crate::values::StarlarkValue;
use crate::values::Tracer;
use crate::values::Value;

/// Untyped raw pointer to `StarlarkValue` without vtable.
///
/// * `'v` lifetime is heap lifetime
/// * `'a` lifetime is value lifetime
#[repr(C)]
pub(crate) struct StarlarkValueRawPtr<'a, 'v> {
    ptr: *const (),
    _phantom: PhantomData<(&'a (), &'v ())>,
}

impl<'a, 'v> StarlarkValueRawPtr<'a, 'v> {
    #[inline]
    fn new(ptr: *const ()) -> Self {
        Self {
            ptr,
            _phantom: PhantomData,
        }
    }

    #[inline]
    pub(crate) unsafe fn value_ref<'w, T: StarlarkValue<'w>>(self) -> &'w T {
        &*(self.ptr as *const T)
    }
}

pub(crate) struct AValueVTable {
    // Common `AValue` fields.
    static_type_of_value: ConstTypeId,
    type_name: &'static str,
    get_hash: fn(*const ()) -> anyhow::Result<StarlarkHashValue>,
    /// Cache `type_name` here to avoid computing hash.
    type_as_allocative_key: allocative::Key,

    // `StarlarkValue`
    starlark_value: StarlarkValueVTable,

    // `Drop`
    drop_in_place: fn(*mut ()),

    // `AValue`
    is_str: bool,
    memory_size: fn(*const ()) -> usize,
    heap_freeze: fn(*mut (), &Freezer) -> anyhow::Result<FrozenValue>,
    heap_copy: for<'v> fn(*mut (), &Tracer<'v>) -> Value<'v>,

    // `StarlarkValue` supertraits.
    display: unsafe fn(*const ()) -> *const dyn Display,
    debug: unsafe fn(*const ()) -> *const dyn Debug,
    erased_serde_serialize: unsafe fn(*const ()) -> *const dyn erased_serde::Serialize,
    allocative: unsafe fn(*const ()) -> *const dyn Allocative,
}

struct GetTypeId<T: ?Sized + 'static>(PhantomData<&'static T>);

impl<T: ?Sized + 'static> GetTypeId<T> {
    const TYPE_ID: ConstTypeId = ConstTypeId::of::<T>();
}

struct GetAllocativeKey<'v, T: StarlarkValue<'v>>(PhantomData<&'v T>);

impl<'v, T: StarlarkValue<'v>> GetAllocativeKey<'v, T> {
    const ALLOCATIVE_KEY: allocative::Key = allocative::Key::new(T::TYPE);
}

impl AValueVTable {
    pub(crate) fn new_black_hole() -> &'static AValueVTable {
        const BLACKHOLE_ALLOCATIVE_KEY: allocative::Key = allocative::Key::new("BlackHole");
        &AValueVTable {
            drop_in_place: |_| {},

            is_str: false,
            memory_size: |p| unsafe { (*(p as *const BlackHole)).0 },
            static_type_of_value: GetTypeId::<BlackHole>::TYPE_ID,

            heap_freeze: |_, _| panic!("BlackHole"),
            heap_copy: |_, _| panic!("BlackHole"),
            get_hash: |_| panic!("BlackHole"),
            type_name: "BlackHole",
            type_as_allocative_key: BLACKHOLE_ALLOCATIVE_KEY,

            display: |this| {
                let this = unsafe { &*(this as *const BlackHole) };
                this as *const dyn Display
            },
            debug: |this| {
                let this = unsafe { &*(this as *const BlackHole) };
                this as *const dyn Debug
            },
            erased_serde_serialize: |this| {
                let this = unsafe { &*(this as *const BlackHole) };
                this as *const dyn erased_serde::Serialize
            },
            allocative: |this| {
                let this = unsafe { &*(this as *const BlackHole) };
                this as *const dyn Allocative
            },
            starlark_value: StarlarkValueVTable::BLACK_HOLE,
        }
    }

    pub(crate) const fn new<'v, T: AValue<'v>>() -> &'static AValueVTable {
        &AValueVTable {
            drop_in_place: |p| unsafe {
                ptr::drop_in_place(p as *mut T);
            },
            is_str: T::IS_STR,
            memory_size: |p| unsafe {
                let p = p as *const T;
                T::memory_size_for_extra_len((*p).extra_len())
            },
            heap_freeze: |p, freezer| unsafe {
                let p = &mut *AValueRepr::from_payload_ptr_mut(p as *mut T);
                T::heap_freeze(p, transmute!(&Freezer, &Freezer, freezer))
            },
            heap_copy: |p, tracer| unsafe {
                let p = &mut *AValueRepr::from_payload_ptr_mut(p as *mut T);
                let value = T::heap_copy(p, transmute!(&Tracer, &Tracer, tracer));
                transmute!(Value, Value, value)
            },
            static_type_of_value:
                GetTypeId::<<T::StarlarkValue as ProvidesStaticType>::StaticType>::TYPE_ID,
            get_hash: |p| unsafe {
                let p = &*(p as *const T);
                T::get_hash(p)
            },
            type_name: T::StarlarkValue::TYPE,
            type_as_allocative_key: GetAllocativeKey::<T::StarlarkValue>::ALLOCATIVE_KEY,
            display: |this| {
                let this = unsafe { &*(this as *const T::StarlarkValue) };
                let display = this as *const dyn Display;
                // Drop lifetime.
                unsafe { mem::transmute(display) }
            },
            debug: |this| {
                let this = unsafe { &*(this as *const T::StarlarkValue) };
                let debug = this as *const dyn Debug;
                // Drop lifetime.
                unsafe { mem::transmute(debug) }
            },
            erased_serde_serialize: |this| {
                let this = unsafe { &*(this as *const T::StarlarkValue) };
                let serialize = this as *const dyn erased_serde::Serialize;
                // Drop lifetime.
                unsafe { mem::transmute(serialize) }
            },
            allocative: |this| {
                let this = unsafe { &*(this as *const T::StarlarkValue) };
                let allocative = this as *const dyn Allocative;
                // Drop lifetime.
                unsafe { mem::transmute(allocative) }
            },
            starlark_value: StarlarkValueVTableGet::<'v, T::StarlarkValue>::VTABLE,
        }
    }

    pub(crate) fn drop_in_place(&self, value: *mut ()) {
        (self.drop_in_place)(value)
    }
}

#[derive(Copy, Clone, Dupe)]
#[repr(C)]
pub(crate) struct AValueDyn<'v> {
    pub(crate) value: &'v (),
    pub(crate) vtable: &'static AValueVTable,
}

impl<'v> AValueDyn<'v> {
    #[inline]
    pub(crate) fn memory_size(self) -> usize {
        let size = (self.vtable.memory_size)(self.value as *const ());
        debug_assert!(size % AValueHeader::ALIGN == 0);
        size
    }

    pub(crate) fn as_allocative(self) -> &'v dyn Allocative {
        unsafe { &*(self.vtable.allocative)(self.value) }
    }

    pub(crate) fn total_memory(self) -> usize {
        mem::size_of::<AValueHeader>()
            + self.memory_size()
            + allocative::size_of_unique_allocated_data(self.as_allocative())
    }

    #[inline]
    pub(crate) fn get_type(self) -> &'static str {
        self.vtable.type_name
    }

    #[inline]
    pub(crate) fn get_type_starlark_repr(self) -> String {
        (self.vtable.starlark_value.get_type_starlark_repr)()
    }

    #[inline]
    pub(crate) fn get_type_value(self) -> FrozenStringValue {
        (self.vtable.starlark_value.get_type_value_static)()
    }

    #[inline]
    pub(crate) fn type_as_allocative_key(self) -> allocative::Key {
        self.vtable.type_as_allocative_key.clone()
    }

    #[inline]
    pub(crate) fn static_type_of_value(self) -> TypeId {
        self.vtable.static_type_of_value.get()
    }

    #[inline]
    pub(crate) fn is_str(self) -> bool {
        self.vtable.is_str
    }

    #[inline]
    pub(crate) unsafe fn heap_freeze(self, freezer: &Freezer) -> anyhow::Result<FrozenValue> {
        (self.vtable.heap_freeze)(self.value as *const _ as *mut (), freezer)
    }

    #[inline]
    pub(crate) unsafe fn heap_copy(self, tracer: &Tracer<'v>) -> Value<'v> {
        (self.vtable.heap_copy)(self.value as *const _ as *mut (), tracer)
    }

    #[inline]
    pub(crate) fn documentation(self) -> Option<DocItem> {
        (self.vtable.starlark_value.documentation)(StarlarkValueRawPtr::new(self.value))
    }

    #[inline]
    pub(crate) fn get_methods(self) -> Option<&'static Methods> {
        (self.vtable.starlark_value.get_methods)()
    }

    #[inline]
    pub(crate) fn at(self, index: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.at)(StarlarkValueRawPtr::new(self.value), index, heap)
    }

    #[inline]
    pub(crate) fn is_in(self, collection: Value<'v>) -> anyhow::Result<bool> {
        (self.vtable.starlark_value.is_in)(StarlarkValueRawPtr::new(self.value), collection)
    }

    #[inline]
    pub(crate) fn slice(
        self,
        start: Option<Value<'v>>,
        stop: Option<Value<'v>>,
        step: Option<Value<'v>>,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.slice)(
            StarlarkValueRawPtr::new(self.value),
            start,
            stop,
            step,
            heap,
        )
    }

    #[inline]
    pub(crate) fn get_attr(self, name: &str, heap: &'v Heap) -> Option<Value<'v>> {
        (self.vtable.starlark_value.get_attr)(StarlarkValueRawPtr::new(self.value), name, heap)
    }

    #[inline]
    pub(crate) fn get_attr_hashed(self, name: Hashed<&str>, heap: &'v Heap) -> Option<Value<'v>> {
        (self.vtable.starlark_value.get_attr_hashed)(
            StarlarkValueRawPtr::new(self.value),
            name,
            heap,
        )
    }

    #[inline]
    pub(crate) fn has_attr(self, name: &str, heap: &'v Heap) -> bool {
        (self.vtable.starlark_value.has_attr)(StarlarkValueRawPtr::new(self.value), name, heap)
    }

    #[inline]
    pub(crate) fn dir_attr(self) -> Vec<String> {
        (self.vtable.starlark_value.dir_attr)(StarlarkValueRawPtr::new(self.value))
    }

    #[inline]
    pub(crate) fn bit_and(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.bit_and)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn bit_or(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.bit_or)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn bit_xor(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.bit_xor)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn bit_not(self, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.bit_not)(StarlarkValueRawPtr::new(self.value), heap)
    }

    #[inline]
    pub(crate) fn to_int(self) -> anyhow::Result<i32> {
        (self.vtable.starlark_value.to_int)(StarlarkValueRawPtr::new(self.value))
    }

    #[inline]
    pub(crate) fn to_bool(self) -> bool {
        (self.vtable.starlark_value.to_bool)(StarlarkValueRawPtr::new(self.value))
    }

    #[inline]
    pub(crate) fn length(self) -> anyhow::Result<i32> {
        (self.vtable.starlark_value.length)(StarlarkValueRawPtr::new(self.value))
    }

    #[inline]
    pub(crate) fn iterate<'a>(
        self,
        heap: &'v Heap,
    ) -> anyhow::Result<Box<dyn Iterator<Item = Value<'v>> + 'v>>
    where
        'v: 'a,
    {
        (self.vtable.starlark_value.iterate)(StarlarkValueRawPtr::new(self.value), heap)
    }

    #[inline]
    pub(crate) fn with_iterator(
        self,
        heap: &'v Heap,
        f: &mut dyn FnMut(&mut dyn Iterator<Item = Value<'v>>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        (self.vtable.starlark_value.with_iterator)(StarlarkValueRawPtr::new(self.value), heap, f)
    }

    #[inline]
    pub(crate) fn get_hash(self) -> anyhow::Result<StarlarkHashValue> {
        (self.vtable.get_hash)(self.value as *const ())
    }

    #[inline]
    pub(crate) fn plus(self, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.plus)(StarlarkValueRawPtr::new(self.value), heap)
    }

    #[inline]
    pub(crate) fn minus(self, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.minus)(StarlarkValueRawPtr::new(self.value), heap)
    }

    #[inline]
    pub(crate) fn add(self, other: Value<'v>, heap: &'v Heap) -> Option<anyhow::Result<Value<'v>>> {
        (self.vtable.starlark_value.add)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn radd(
        self,
        other: Value<'v>,
        heap: &'v Heap,
    ) -> Option<anyhow::Result<Value<'v>>> {
        (self.vtable.starlark_value.radd)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn sub(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.sub)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn mul(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.mul)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn div(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.div)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn floor_div(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.floor_div)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn percent(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.percent)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn left_shift(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.left_shift)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn right_shift(self, other: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.right_shift)(StarlarkValueRawPtr::new(self.value), other, heap)
    }

    #[inline]
    pub(crate) fn collect_repr(self, collector: &mut String) {
        (self.vtable.starlark_value.collect_repr)(StarlarkValueRawPtr::new(self.value), collector)
    }

    #[inline]
    pub(crate) fn collect_repr_cycle(self, collector: &mut String) {
        (self.vtable.starlark_value.collect_repr_cycle)(
            StarlarkValueRawPtr::new(self.value),
            collector,
        )
    }

    #[inline]
    pub(crate) fn downcast_ref<T: StarlarkValue<'v>>(self) -> Option<&'v T> {
        if self.static_type_of_value() == T::static_type_id() {
            // SAFETY: just checked whether we are pointing to the correct type.
            unsafe { Some(&*(self.value as *const () as *const T)) }
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn equals(self, other: Value<'v>) -> anyhow::Result<bool> {
        (self.vtable.starlark_value.equals)(StarlarkValueRawPtr::new(self.value), other)
    }

    #[inline]
    pub(crate) fn compare(self, other: Value<'v>) -> anyhow::Result<Ordering> {
        (self.vtable.starlark_value.compare)(StarlarkValueRawPtr::new(self.value), other)
    }

    #[inline]
    pub(crate) fn invoke(
        self,
        me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_>,
    ) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.invoke)(StarlarkValueRawPtr::new(self.value), me, args, eval)
    }

    #[inline]
    pub(crate) fn invoke_method(
        self,
        me: Value<'v>,
        this: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_>,
    ) -> anyhow::Result<Value<'v>> {
        (self.vtable.starlark_value.invoke_method)(
            StarlarkValueRawPtr::new(self.value),
            me,
            this,
            args,
            eval,
            Private,
        )
    }

    #[inline]
    pub(crate) fn name_for_call_stack(self, me: Value<'v>) -> String {
        (self.vtable.starlark_value.name_for_call_stack)(StarlarkValueRawPtr::new(self.value), me)
    }

    #[inline]
    pub(crate) fn export_as(self, variable_name: &str, eval: &mut Evaluator<'v, '_>) {
        (self.vtable.starlark_value.export_as)(
            StarlarkValueRawPtr::new(self.value),
            variable_name,
            eval,
        )
    }

    #[inline]
    pub(crate) fn set_at(self, index: Value<'v>, new_value: Value<'v>) -> anyhow::Result<()> {
        (self.vtable.starlark_value.set_at)(StarlarkValueRawPtr::new(self.value), index, new_value)
    }

    #[inline]
    pub(crate) fn set_attr(self, attribute: &str, new_value: Value<'v>) -> anyhow::Result<()> {
        (self.vtable.starlark_value.set_attr)(
            StarlarkValueRawPtr::new(self.value),
            attribute,
            new_value,
        )
    }

    #[inline]
    pub(crate) fn write_hash(self, hasher: &mut StarlarkHasher) -> anyhow::Result<()> {
        (self.vtable.starlark_value.write_hash)(StarlarkValueRawPtr::new(self.value), hasher)
    }

    #[inline]
    pub(crate) fn matches_type(self, t: &str) -> bool {
        (self.vtable.starlark_value.matches_type)(StarlarkValueRawPtr::new(self.value), t)
    }

    #[inline]
    pub(crate) fn as_display(self) -> &'v dyn Display {
        unsafe { &*(self.vtable.display)(self.value) }
    }

    #[inline]
    pub(crate) fn as_debug(self) -> &'v dyn Debug {
        unsafe { &*(self.vtable.debug)(self.value) }
    }

    #[inline]
    pub(crate) fn as_serialize(self) -> &'v dyn erased_serde::Serialize {
        unsafe { &*(self.vtable.erased_serde_serialize)(self.value) }
    }

    #[inline]
    pub(crate) fn provide(self, demand: &mut Demand<'_, 'v>) {
        (self.vtable.starlark_value.provide)(StarlarkValueRawPtr::new(self.value), demand)
    }
}
