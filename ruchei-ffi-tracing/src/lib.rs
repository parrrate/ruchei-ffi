use std::{cell::RefCell, collections::HashMap, fmt::Debug, num::NonZeroU64, sync::Mutex};

use abi_stable::{
    sabi_trait::TD_Opaque,
    std_types::{
        RArc, ROption,
        RResult::{self, RErr, ROk},
        RSlice, RStr, Tuple2,
    },
    traits::{IntoReprC, IntoReprRust},
    DynTrait, RMut, RRef, StableAbi,
};
use bumpalo::Bump;
use ruchei_ffi_registry::Registry;
use tracing_core::{
    callsite::Identifier,
    field::{FieldSet, Value, ValueSet, Visit},
    identify_callsite,
    span::{Attributes, Current, Id, Record},
    Callsite, Dispatch, Event, Field, Interest, Kind, Level, LevelFilter, Metadata, Subscriber,
};

#[repr(C)]
#[derive(StableAbi)]
#[sabi(impl_InterfaceType(FmtWrite))]
pub struct FmtInterface;

#[abi_stable::sabi_trait]
pub trait RDebug {
    fn debug(&self, f: &'_ mut DynTrait<'_, RMut<'_, ()>, FmtInterface>) -> RResult<(), ()>;
}

struct RDebugged<T>(T);

impl<T: Debug> RDebug for RDebugged<T> {
    fn debug(&self, f: &'_ mut DynTrait<'_, RMut<'_, ()>, FmtInterface>) -> RResult<(), ()> {
        use std::fmt::Write;
        match write!(f, "{:?}", self.0) {
            Ok(()) => ROk(()),
            Err(std::fmt::Error) => RErr(()),
        }
    }
}

impl Debug for RDebug_TO<'_, RRef<'_, ()>> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.debug(&mut DynTrait::from_borrowing_ptr(f)) {
            RResult::ROk(()) => Ok(()),
            RResult::RErr(()) => Err(std::fmt::Error),
        }
    }
}

#[repr(C)]
#[derive(StableAbi)]
#[sabi(impl_InterfaceType(Debug))]
pub struct DebugInterface;

#[repr(C)]
#[derive(StableAbi, Clone, Copy)]
pub struct RField {
    name: RStr<'static>,
    fields: RFieldSet,
}

trait ToField {
    fn field(&self) -> RField;
}

impl ToField for Field {
    fn field(&self) -> RField {
        let identifier = self.callsite();
        RField {
            name: self.name().into_c(),
            fields: RFieldSet {
                names: RNAMES
                    .try_get(&identifier)
                    .expect("unknown callsite")
                    .into_c(),
                callsite: identifier.into(),
            },
        }
    }
}

impl RField {
    fn field(&self) -> Field {
        self.fields
            .fields()
            .field(self.name.as_str())
            .expect("unknown field")
    }
}

#[abi_stable::sabi_trait]
pub trait RVisit {
    fn record_debug(
        &mut self,
        field: &RField,
        value: &'_ DynTrait<'_, RRef<'_, ()>, DebugInterface>,
    );

    fn visitor<'b>(&'b mut self) -> RVisitor<'b>;
}

impl RVisit for &mut dyn Visit {
    fn record_debug(
        &mut self,
        field: &RField,
        value: &'_ DynTrait<'_, RRef<'_, ()>, DebugInterface>,
    ) {
        (*self).record_debug(&field.field(), value)
    }

    fn visitor(&mut self) -> RVisitor {
        RVisitor(RVisit_TO::from_ptr(self, TD_Opaque))
    }
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct RVisitor<'a>(RVisit_TO<'a, RMut<'a, ()>>);

impl Visit for RVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0
            .record_debug(&field.field(), &DynTrait::from_borrowing_ptr(&value))
    }
}

#[abi_stable::sabi_trait]
pub trait RValue {
    fn record(&self, key: &RField, visitor: &mut RVisitor<'_>);
}

impl<T: ?Sized + Value> RValue for T {
    fn record(&self, key: &RField, visitor: &mut RVisitor<'_>) {
        self.record(&key.field(), visitor)
    }
}

#[repr(u8)]
#[derive(StableAbi, Clone, Copy)]
pub enum RInterest {
    Never = 0,
    Sometimes = 1,
    Always = 2,
}

impl From<Interest> for RInterest {
    fn from(value: Interest) -> Self {
        if value.is_always() {
            Self::Always
        } else if value.is_sometimes() {
            Self::Sometimes
        } else {
            debug_assert!(value.is_never());
            Self::Never
        }
    }
}

impl From<RInterest> for Interest {
    fn from(value: RInterest) -> Self {
        match value {
            RInterest::Never => Self::never(),
            RInterest::Sometimes => Self::sometimes(),
            RInterest::Always => Self::always(),
        }
    }
}

#[repr(transparent)]
#[derive(StableAbi, Clone, Copy)]
pub struct RKind(u8);

impl RKind {
    const EVENT_BIT: u8 = 1 << 0;
    const SPAN_BIT: u8 = 1 << 1;
    const HINT_BIT: u8 = 1 << 2;

    pub fn is_span(&self) -> bool {
        self.0 & Self::SPAN_BIT == Self::SPAN_BIT
    }

    pub fn is_event(&self) -> bool {
        self.0 & Self::EVENT_BIT == Self::EVENT_BIT
    }

    pub fn is_hint(&self) -> bool {
        self.0 & Self::HINT_BIT == Self::HINT_BIT
    }

    fn event(self, event: bool) -> Self {
        if event {
            Self(self.0 | Self::EVENT_BIT)
        } else {
            self
        }
    }

    fn span(self, span: bool) -> Self {
        if span {
            Self(self.0 | Self::SPAN_BIT)
        } else {
            self
        }
    }

    fn hint(self, hint: bool) -> Self {
        if hint {
            Self(self.0 | Self::HINT_BIT)
        } else {
            self
        }
    }
}

impl From<Kind> for RKind {
    fn from(value: Kind) -> Self {
        Self(0)
            .event(value.is_event())
            .span(value.is_span())
            .hint(value.is_hint())
    }
}

impl From<RKind> for Kind {
    fn from(value: RKind) -> Self {
        if value.is_event() {
            if value.is_hint() {
                Self::EVENT.hint()
            } else {
                Self::EVENT
            }
        } else if value.is_span() {
            if value.is_hint() {
                Self::SPAN.hint()
            } else {
                Self::SPAN
            }
        } else {
            debug_assert!(value.is_hint());
            Self::HINT
        }
    }
}

struct RCallsiteBlock;

impl RCallsite for RCallsiteBlock {
    fn set_interest(&self, interest: RInterest) {
        let _ = interest;
        unimplemented!()
    }

    fn metadata(&self) -> &RMetadata<'_> {
        unimplemented!()
    }
}

fn rcallsite() -> RCallsite_TO<RRef<'static, ()>> {
    static RCALLSITEBLOCK: RCallsiteBlock = RCallsiteBlock;

    RCallsite_TO::from_const(&RCALLSITEBLOCK, TD_Opaque)
}

#[abi_stable::sabi_trait]
pub trait RCallsite: 'static + Sync {
    fn set_interest(&self, interest: RInterest);

    fn metadata(&self) -> &RMetadata<'_>;
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct RDynCallsite {
    callsite: &'static RCallsite_TO<RRef<'static, ()>>,
}

impl Callsite for RDynCallsite {
    fn set_interest(&self, interest: Interest) {
        self.callsite.set_interest(interest.into())
    }

    fn metadata(&self) -> &Metadata<'_> {
        self.callsite.metadata().metadata_ref()
    }
}

#[repr(transparent)]
#[derive(StableAbi, Clone, Copy)]
pub struct RIdentifier {
    callsite: &'static RDynCallsite,
}

impl From<Identifier> for RIdentifier {
    fn from(value: Identifier) -> Self {
        static CALLSITES: Registry<Identifier, RDynCallsite> = Registry::new();

        Self {
            callsite: CALLSITES.get(value, || {
                Box::new(RDynCallsite {
                    callsite: Box::leak(Box::new(rcallsite())),
                })
            }),
        }
    }
}

impl From<RIdentifier> for Identifier {
    fn from(value: RIdentifier) -> Self {
        identify_callsite!(value.callsite)
    }
}

#[repr(C)]
#[derive(StableAbi, Clone, Copy)]
pub struct RFieldSet {
    names: RSlice<'static, RStr<'static>>,
    callsite: RIdentifier,
}

trait ToFieldSet {
    fn fields(&self, identifier: Identifier) -> RFieldSet;
    fn names(&self, identifier: Identifier) -> RSlice<'static, RStr<'static>>;
}

static RNAMES: Registry<Identifier, [RStr<'static>]> = Registry::new();

impl ToFieldSet for FieldSet {
    fn fields(&self, identifier: Identifier) -> RFieldSet {
        RFieldSet {
            names: self.names(identifier.clone()),
            callsite: identifier.into(),
        }
    }

    fn names(&self, identifier: Identifier) -> RSlice<'static, RStr<'static>> {
        RNAMES
            .get(identifier, || {
                self.iter().map(|field| field.name().into_c()).collect()
            })
            .into_c()
    }
}

impl RFieldSet {
    fn fields(&self) -> FieldSet {
        FieldSet::new(self.names(), self.callsite.into())
    }

    fn names(&self) -> &'static [&'static str] {
        static NAMES: Registry<&'static [RStr<'static>], [&'static str]> = Registry::new();

        NAMES.get(self.names.as_slice(), || {
            self.names.iter().map(|name| name.as_str()).collect()
        })
    }
}

#[repr(C)]
#[derive(StableAbi)]
pub struct RMetadata<'a> {
    name: RStr<'static>,
    target: RStr<'a>,
    level: RLevel,
    module_path: ROption<RStr<'a>>,
    file: ROption<RStr<'a>>,
    line: ROption<u32>,
    fields: RFieldSet,
    kind: RKind,
}

impl<'a> RMetadata<'a> {
    fn metadata(&self) -> Metadata<'a> {
        Metadata::new(
            self.name.as_str(),
            self.target.as_str(),
            (&self.level).into(),
            self.file.into_rust().map(|o| o.as_str()),
            self.line.into_rust(),
            self.file.into_rust().map(|o| o.as_str()),
            self.fields.fields(),
            self.kind.into(),
        )
    }
}

trait AsValues {
    fn record(&self, visitor: &mut dyn Visit);
}

impl AsValues for ValueSet<'_> {
    fn record(&self, visitor: &mut dyn Visit) {
        self.record(visitor)
    }
}

impl AsValues for Record<'_> {
    fn record(&self, visitor: &mut dyn Visit) {
        self.record(visitor)
    }
}

impl AsValues for Event<'_> {
    fn record(&self, visitor: &mut dyn Visit) {
        self.record(visitor)
    }
}

const MAX_VALUES: usize = 256;

impl RMetadata<'static> {
    fn metadata_ref(&'static self) -> &'static Metadata<'static> {
        static METADATAS: Registry<usize, Metadata<'static>> = Registry::new();

        METADATAS.get((self as *const Self) as usize, || Box::new(self.metadata()))
    }

    fn values<T>(&'static self, values: &impl AsValues, f: impl FnOnce(&RValueSet) -> T) -> T {
        thread_local! {
            static BUMP: RefCell<Bump> = RefCell::new(Bump::new());
        }

        struct FieldVisit<'a, 'b>(
            &'a Field,
            &'a mut Option<std::fmt::Result>,
            &'a mut std::fmt::Formatter<'b>,
        );

        impl Visit for FieldVisit<'_, '_> {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if self.1.is_none() && field == self.0 {
                    *self.1 = Some(value.fmt(self.2));
                }
            }
        }

        struct FieldDebug<'a>(Field, &'a dyn AsValues);

        impl Debug for FieldDebug<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut res = None;
                self.1.record(&mut FieldVisit(&self.0, &mut res, f));
                res.expect("field/value existence changed???")
            }
        }

        struct BumpVisit<'store, 'bump, 'borrow, 'visit>(
            &'borrow mut bumpalo::collections::Vec<
                'bump,
                Tuple2<&'store RField, RDebug_TO<'store, RRef<'store, ()>>>,
            >,
            &'visit dyn AsValues,
        );

        impl<'store, 'bump: 'store, 'visit: 'store> Visit for BumpVisit<'store, 'bump, '_, 'visit> {
            fn record_debug(&mut self, field: &Field, _: &dyn std::fmt::Debug) {
                if self.0.len() < MAX_VALUES {
                    let field_ref = &*self.0.bump().alloc(field.field());
                    let value_ref = &*self
                        .0
                        .bump()
                        .alloc(RDebugged(FieldDebug(field.clone(), self.1)));
                    let value_ref = RDebug_TO::from_ptr(value_ref, TD_Opaque);
                    self.0.push(Tuple2(field_ref, value_ref));
                }
            }
        }

        BUMP.with_borrow_mut(|bump| {
            let mut tuples = bumpalo::vec![in bump];
            values.record(&mut BumpVisit(&mut tuples, values));
            let ret = f(&RValueSet {
                values: tuples.as_slice().into(),
                fields: &self.fields,
            });
            drop(tuples);
            bump.reset();
            ret
        })
    }
}

trait ToMetadata<'a> {
    fn metadata(&self) -> RMetadata<'a>;
}

impl<'a> ToMetadata<'a> for Metadata<'a> {
    fn metadata(&self) -> RMetadata<'a> {
        RMetadata {
            name: self.name().into(),
            target: self.target().into(),
            level: self.level().into(),
            module_path: self.module_path().map(Into::into).into(),
            file: self.module_path().map(Into::into).into(),
            line: self.line().into(),
            fields: self.fields().fields(self.callsite()),
            kind: RKind(0).event(self.is_event()).span(self.is_span()),
        }
    }
}

trait ToMetadataRef {
    fn metadata_ref(&'static self) -> &'static RMetadata<'static>;
}

impl ToMetadataRef for Metadata<'static> {
    fn metadata_ref(&'static self) -> &'static RMetadata<'static> {
        static RMETADATAS: Registry<usize, RMetadata<'static>> = Registry::new();

        RMETADATAS.get((self as *const Self) as usize, || Box::new(self.metadata()))
    }
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct RId(NonZeroU64);

impl RId {
    fn id(&self) -> Id {
        Id::from_non_zero_u64(self.0)
    }
}

trait ToId {
    fn id(&self) -> RId;
}

impl ToId for Id {
    fn id(&self) -> RId {
        RId(self.into_non_zero_u64())
    }
}

impl From<Id> for RId {
    fn from(value: Id) -> Self {
        value.id()
    }
}

impl From<RId> for Id {
    fn from(value: RId) -> Self {
        value.id()
    }
}

/// Passing values is w.i.p.
#[repr(C)]
#[derive(StableAbi)]
pub struct RValueSet<'a, 'b> {
    values: RSlice<'a, Tuple2<&'a RField, RDebug_TO<'a, RRef<'b, ()>>>>,
    fields: &'a RFieldSet,
}

fn array_slice_from<'a, const N: usize>(
    slice: &'a [(&'a Field, Option<&'a (dyn Value + 'a)>)],
) -> &'a [(&'a Field, Option<&'a (dyn Value + 'a)>); N] {
    slice.try_into().unwrap()
}

fn value_set<'a>(fields: &'a FieldSet, tuples: &'a [(&Field, Option<&dyn Value>)]) -> ValueSet<'a> {
    const TOO_MANY_VALUES: usize = MAX_VALUES + 1;
    macro_rules! arms {
        ($($x:literal,)*) => (
            match tuples.len() {
                $($x => fields.value_set(array_slice_from::<$x>(tuples)),)*
                MAX_VALUES => fields.value_set(array_slice_from::<MAX_VALUES>(tuples)),
                TOO_MANY_VALUES.. => fields.value_set(array_slice_from::<MAX_VALUES>(&tuples[..MAX_VALUES])),
            }
        )
    }
    arms!(
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c,
        0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b,
        0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a,
        0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
        0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
        0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77,
        0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86,
        0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95,
        0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4,
        0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb2, 0xb3,
        0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf, 0xc0, 0xc1, 0xc2,
        0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf, 0xd0, 0xd1,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0xde, 0xdf, 0xe0,
        0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef,
        0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe,
        0xff,
    )
}

impl RValueSet<'_, '_> {
    fn values<T>(&self, f: impl FnOnce(&ValueSet) -> T) -> T {
        thread_local! {
            static BUMP: RefCell<Bump> = RefCell::new(Bump::new());
        }

        BUMP.with_borrow_mut(|bump| {
            let mut tuples = bumpalo::vec![in bump];
            for Tuple2(field, value) in self.values.iter().take(MAX_VALUES) {
                let field_ref = &*bump.alloc(field.field());
                let value_ref = bump.alloc(tracing_core::field::debug(value)) as &dyn Value;
                tuples.push((field_ref, Some(value_ref)));
            }
            let fields = self.fields.fields();
            let ret = f(&value_set(&fields, &tuples));
            drop(tuples);
            bump.reset();
            ret
        })
    }
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct RRecord<'a, 'b> {
    values: &'a RValueSet<'a, 'b>,
}

impl RRecord<'_, '_> {
    fn record<T>(&self, f: impl FnOnce(&Record) -> T) -> T {
        self.values.values(|values| f(&Record::new(values)))
    }
}

#[repr(u8)]
#[derive(StableAbi)]
pub enum RParent {
    Root,
    Current,
    Explicit(RId),
}

#[repr(C)]
#[derive(StableAbi)]
pub struct RAttributes<'a, 'b> {
    metadata: &'static RMetadata<'static>,
    values: &'a RValueSet<'a, 'b>,
    parent: RParent,
}

trait ToAttributes {
    fn attributes<T>(&self, f: impl FnOnce(&RAttributes) -> T) -> T;
}

impl ToAttributes for Attributes<'_> {
    fn attributes<T>(&self, f: impl FnOnce(&RAttributes) -> T) -> T {
        let metadata = self.metadata().metadata_ref();
        metadata.values(self.values(), |values| {
            f(&RAttributes {
                metadata,
                values,
                parent: if let Some(parent) = self.parent() {
                    debug_assert!(!self.is_contextual());
                    debug_assert!(!self.is_root());
                    RParent::Explicit(parent.id())
                } else if self.is_contextual() {
                    debug_assert!(!self.is_root());
                    RParent::Current
                } else {
                    debug_assert!(self.is_root());
                    RParent::Root
                },
            })
        })
    }
}

impl RAttributes<'_, '_> {
    fn attributes<T>(&self, f: impl FnOnce(&Attributes) -> T) -> T {
        let metadata = self.metadata.metadata_ref();
        self.values.values(|values| {
            f(&match &self.parent {
                RParent::Root => Attributes::new_root(metadata, values),
                RParent::Current => Attributes::new(metadata, values),
                RParent::Explicit(parent) => Attributes::child_of(parent.id(), metadata, values),
            })
        })
    }
}

#[repr(C)]
#[derive(StableAbi)]
pub struct REvent<'a, 'b> {
    values: &'a RValueSet<'a, 'b>,
    metadata: &'static RMetadata<'static>,
    parent: RParent,
}

trait ToEvent {
    fn event<T>(&self, f: impl FnOnce(&REvent) -> T) -> T;
}

impl ToEvent for Event<'_> {
    fn event<T>(&self, f: impl FnOnce(&REvent) -> T) -> T {
        let metadata = self.metadata().metadata_ref();
        metadata.values(self, |values| {
            f(&REvent {
                values,
                metadata,
                parent: if let Some(parent) = self.parent() {
                    debug_assert!(!self.is_contextual());
                    debug_assert!(!self.is_root());
                    RParent::Explicit(parent.id())
                } else if self.is_contextual() {
                    debug_assert!(!self.is_root());
                    RParent::Current
                } else {
                    debug_assert!(self.is_root());
                    RParent::Root
                },
            })
        })
    }
}

impl REvent<'_, '_> {
    fn event<T>(&self, f: impl FnOnce(&Event) -> T) -> T {
        let metadata = self.metadata.metadata_ref();
        self.values.values(|fields| {
            f(&match &self.parent {
                RParent::Root => Event::new_child_of(None, metadata, fields),
                RParent::Current => Event::new(metadata, fields),
                RParent::Explicit(id) => Event::new_child_of(Some(id.id()), metadata, fields),
            })
        })
    }
}

#[repr(u8)]
#[derive(StableAbi)]
pub enum RLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<&Level> for RLevel {
    fn from(value: &Level) -> Self {
        match *value {
            Level::ERROR => Self::Error,
            Level::WARN => Self::Warn,
            Level::INFO => Self::Info,
            Level::DEBUG => Self::Debug,
            Level::TRACE => Self::Trace,
        }
    }
}

impl From<&RLevel> for Level {
    fn from(value: &RLevel) -> Self {
        match value {
            RLevel::Error => Self::ERROR,
            RLevel::Warn => Self::WARN,
            RLevel::Info => Self::INFO,
            RLevel::Debug => Self::DEBUG,
            RLevel::Trace => Self::TRACE,
        }
    }
}

#[repr(u8)]
#[derive(StableAbi)]
pub enum RLevelFilter {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LevelFilter> for RLevelFilter {
    fn from(value: LevelFilter) -> Self {
        match value {
            LevelFilter::OFF => Self::Off,
            LevelFilter::ERROR => Self::Error,
            LevelFilter::WARN => Self::Warn,
            LevelFilter::INFO => Self::Info,
            LevelFilter::DEBUG => Self::Debug,
            LevelFilter::TRACE => Self::Trace,
        }
    }
}

impl From<RLevelFilter> for LevelFilter {
    fn from(value: RLevelFilter) -> Self {
        match value {
            RLevelFilter::Off => Self::OFF,
            RLevelFilter::Error => Self::ERROR,
            RLevelFilter::Warn => Self::WARN,
            RLevelFilter::Info => Self::INFO,
            RLevelFilter::Debug => Self::DEBUG,
            RLevelFilter::Trace => Self::TRACE,
        }
    }
}

#[repr(u8)]
#[derive(StableAbi)]
pub enum RCurrent {
    Current {
        id: RId,
        metadata: &'static RMetadata<'static>,
    },
    None,
    Unknown,
}

impl From<Current> for RCurrent {
    fn from(value: Current) -> Self {
        let known = value.is_known();
        if let Some((id, metadata)) = value.into_inner() {
            Self::Current {
                id: id.into(),
                metadata: metadata.metadata_ref(),
            }
        } else if known {
            Self::None
        } else {
            Self::Unknown
        }
    }
}

impl From<RCurrent> for Current {
    fn from(value: RCurrent) -> Self {
        struct Temp;
        impl Subscriber for Temp {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                unreachable!()
            }
            fn new_span(&self, _: &Attributes<'_>) -> Id {
                unreachable!()
            }
            fn record(&self, _: &Id, _: &Record<'_>) {
                unreachable!()
            }
            fn record_follows_from(&self, _: &Id, _: &Id) {
                unreachable!()
            }
            fn event(&self, _: &Event<'_>) {
                unreachable!()
            }
            fn enter(&self, _: &Id) {
                unreachable!()
            }
            fn exit(&self, _: &Id) {
                unreachable!()
            }
        }
        match value {
            RCurrent::Current { id, metadata } => Current::new(id.into(), metadata.metadata_ref()),
            RCurrent::None => Current::none(),
            RCurrent::Unknown => Subscriber::current_span(&Temp),
        }
    }
}

#[abi_stable::sabi_trait]
pub trait RSubscriber: 'static + Send + Sync {
    fn enabled(&self, metadata: &RMetadata<'_>) -> bool;
    fn new_span(&self, span: &RAttributes) -> RId;
    fn record(&self, span: &RId, values: &RRecord);
    fn record_follows_from(&self, span: &RId, follows: &RId);
    fn event(&self, event: &REvent);
    fn enter(&self, span: &RId);
    fn exit(&self, span: &RId);
    fn register_callsite(&self, metadata: &'static RMetadata<'static>) -> RInterest;
    fn max_level_hint(&self) -> ROption<RLevelFilter>;
    fn event_enabled(&self, event: &REvent) -> bool;
    fn clone_span(&self, id: &RId) -> RId;
    fn try_close(&self, id: RId) -> bool;
    fn current_span(&self) -> RCurrent;
}

impl<T: Subscriber + Send + Sync> RSubscriber for T {
    fn enabled(&self, metadata: &RMetadata<'_>) -> bool {
        self.enabled(&metadata.metadata())
    }

    fn new_span(&self, span: &RAttributes) -> RId {
        span.attributes(|span| self.new_span(span)).into()
    }

    fn record(&self, span: &RId, values: &RRecord) {
        values.record(|values| self.record(&span.id(), values))
    }

    fn record_follows_from(&self, span: &RId, follows: &RId) {
        self.record_follows_from(&span.id(), &follows.id())
    }

    fn event(&self, event: &REvent) {
        event.event(|event| self.event(event))
    }

    fn enter(&self, span: &RId) {
        self.enter(&span.id())
    }

    fn exit(&self, span: &RId) {
        self.exit(&span.id())
    }

    fn register_callsite(&self, metadata: &'static RMetadata<'static>) -> RInterest {
        self.register_callsite(metadata.metadata_ref()).into()
    }

    fn max_level_hint(&self) -> ROption<RLevelFilter> {
        self.max_level_hint().map(Into::into).into_c()
    }

    fn event_enabled(&self, event: &REvent) -> bool {
        event.event(|event| self.event_enabled(event))
    }

    fn clone_span(&self, id: &RId) -> RId {
        self.clone_span(&id.id()).into()
    }

    fn try_close(&self, id: RId) -> bool {
        self.try_close(id.into())
    }

    fn current_span(&self) -> RCurrent {
        self.current_span().into()
    }
}

#[repr(C)]
#[derive(StableAbi)]
pub struct RSubscriberArc(RSubscriber_TO<RArc<()>>);

impl Unpin for RSubscriberArc {}

impl Clone for RSubscriberArc {
    fn clone(&self) -> Self {
        Self(RSubscriber_TO::<RArc<()>>::from_sabi(
            self.0.obj.shallow_clone(),
        ))
    }
}

impl RSubscriberArc {
    pub fn new(subscriber: impl Subscriber + Send + Sync) -> Self {
        Self(RSubscriber_TO::from_ptr(RArc::new(subscriber), TD_Opaque))
    }

    pub fn dispatch(self) -> Dispatch {
        Dispatch::new(RArcSubscriber(self.0, Default::default()))
    }
}

impl<T: Subscriber + Send + Sync> From<T> for RSubscriberArc {
    fn from(subscriber: T) -> Self {
        Self::new(subscriber)
    }
}

struct RArcSubscriber(
    RSubscriber_TO<RArc<()>>,
    Mutex<HashMap<Id, &'static Metadata<'static>>>,
);

impl Subscriber for RArcSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.0.enabled(&metadata.metadata())
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        let id = span.attributes(|span| self.0.new_span(span)).id();
        self.1.lock().unwrap().insert(id.clone(), span.metadata());
        id
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        if let Some(metadata) = self.1.lock().unwrap().get(span).copied() {
            metadata.metadata_ref().values(values, |values| {
                self.0.record(&span.id(), &RRecord { values })
            })
        }
    }

    fn record_follows_from(&self, span: &Id, follows: &Id) {
        self.0.record_follows_from(&span.id(), &follows.id())
    }

    fn event(&self, event: &Event<'_>) {
        event.event(|event| self.0.event(event))
    }

    fn enter(&self, span: &Id) {
        self.0.enter(&span.id())
    }

    fn exit(&self, span: &Id) {
        self.0.exit(&span.id())
    }

    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.0.register_callsite(metadata.metadata_ref()).into()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.0.max_level_hint().into_rust().map(Into::into)
    }

    fn event_enabled(&self, event: &Event<'_>) -> bool {
        event.event(|event| self.0.event_enabled(event))
    }

    fn clone_span(&self, id: &Id) -> Id {
        self.0.clone_span(&id.id()).into()
    }

    fn try_close(&self, id: Id) -> bool {
        let dropped = self.0.try_close(id.id());
        if dropped {
            self.1.lock().unwrap().remove(&id);
        }
        dropped
    }

    fn current_span(&self) -> Current {
        self.0.current_span().into()
    }
}
