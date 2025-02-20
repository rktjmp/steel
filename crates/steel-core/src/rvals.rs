pub mod cycles;

use crate::{
    gc::{unsafe_erased_pointers::OpaqueReference, Gc},
    parser::{
        ast::{self, Atom, ExprKind},
        parser::SyntaxObject,
        span::Span,
        tokens::TokenType,
    },
    rerrs::{ErrorKind, SteelErr},
    steel_vm::vm::{BuiltInSignature, Continuation},
    values::port::SteelPort,
    values::{
        contracts::{ContractType, ContractedFunction},
        functions::ByteCodeLambda,
        lazy_stream::LazyStream,
        transducers::{Reducer, Transducer},
    },
    values::{functions::BoxedDynFunction, structs::UserDefinedStruct},
};

// #[cfg(feature = "jit")]
// use crate::jit::sig::JitFunctionPointer;

use std::{
    any::Any,
    cell::{Ref, RefCell, RefMut},
    cmp::Ordering,
    convert::TryInto,
    fmt,
    future::Future,
    hash::{Hash, Hasher},
    ops::Deref,
    pin::Pin,
    rc::Rc,
    result,
    task::Context,
};

use std::vec::IntoIter;

// TODO
#[macro_export]
macro_rules! list {
    () => { $crate::rvals::SteelVal::ListV(
        im_lists::list![]
    ) };

    ( $($x:expr),* ) => {{
        $crate::rvals::SteelVal::ListV(im_lists::list![$(
            $crate::rvals::IntoSteelVal::into_steelval($x).unwrap()
        ), *])
    }};

    ( $($x:expr ,)* ) => {{
        $crate::rvals::SteelVal::ListV(im_lists::list![$(
            $crate::rvals::IntoSteelVal::into_steelval($x).unwrap()
        )*])
    }};
}

use SteelVal::*;

use im_rc::{HashMap, Vector};

use futures_task::noop_waker_ref;
use futures_util::future::Shared;
use futures_util::FutureExt;

use im_lists::list::List;
use steel_parser::tokens::MaybeBigInt;

use self::cycles::CycleDetector;

pub type RcRefSteelVal = Rc<RefCell<SteelVal>>;
pub fn new_rc_ref_cell(x: SteelVal) -> RcRefSteelVal {
    Rc::new(RefCell::new(x))
}

pub type Result<T> = result::Result<T, SteelErr>;
pub type FunctionSignature = fn(&[SteelVal]) -> Result<SteelVal>;
pub type MutFunctionSignature = fn(&mut [SteelVal]) -> Result<SteelVal>;
// pub type FunctionSignature = fn(&[SteelVal]) -> Result<SteelVal>;

// TODO: This increases the size of the SteelVal enum by 8 bytes. Consider boxing it instead
pub type BoxedFunctionSignature = Rc<Box<dyn Fn(&[SteelVal]) -> Result<SteelVal>>>;

pub type BoxedAsyncFunctionSignature = Box<Rc<dyn Fn(&[SteelVal]) -> Result<FutureResult>>>;

// Do something like this:
// vector of async functions
// then for a wait group, make a closure that looks something like this:
// async move vec<functioncalls> |_| {
//    let values = Vec::new();
//    for func in vec {
//         values.push(func(args).await)
//    }
//    values
// }

// pub type BoxedFutureResult = Shared<Output = Result<Gc<SteelVal>>>;
pub type AsyncSignature = fn(&[SteelVal]) -> FutureResult;

pub type BoxedFutureResult = Pin<Box<dyn Future<Output = Result<SteelVal>>>>;

// Pin<Box<dyn Future<Output = T> + 'a + Send>>;

#[derive(Clone)]
pub struct FutureResult(Shared<BoxedFutureResult>);

impl FutureResult {
    pub fn new(fut: BoxedFutureResult) -> Self {
        // FutureResult()
        FutureResult(fut.shared())
    }

    pub fn into_shared(self) -> Shared<BoxedFutureResult> {
        self.0
    }
}

// This is an attempt to one off poll a future
// This should enable us to use embedded async functions
// Will require using call/cc w/ a thread queue in steel, however it should be possible
pub(crate) fn poll_future(mut fut: Shared<BoxedFutureResult>) -> Option<Result<SteelVal>> {
    // If the future has already been awaited (by somebody) get that value instead
    if let Some(output) = fut.peek() {
        return Some(output.clone());
    }

    // Otherwise, go ahead and poll the value to see if its ready
    // The context is going to exist exclusively in Steel, hidden behind an `await`
    let waker = noop_waker_ref();
    let context = &mut Context::from_waker(waker);

    // Polling requires a pinned future - TODO make sure this is correct
    let mut_fut = Pin::new(&mut fut);

    match Future::poll(mut_fut, context) {
        std::task::Poll::Ready(r) => Some(r),
        std::task::Poll::Pending => None,
    }
}

/// Attempt to cast this custom type down to the underlying type
pub fn as_underlying_type<T: 'static>(value: &dyn CustomType) -> Option<&T> {
    value.as_any_ref().downcast_ref::<T>()
}

pub trait Custom: private::Sealed {
    fn fmt(&self) -> Option<std::result::Result<String, std::fmt::Error>> {
        None
    }

    fn into_serializable_steelval(&mut self) -> Option<SerializableSteelVal> {
        None
    }
}

pub trait CustomType {
    // fn box_clone(&self) -> Box<dyn CustomType>;
    // fn as_any(&self) -> Box<dyn Any>;
    fn as_any_ref(&self) -> &dyn Any;
    fn as_any_ref_mut(&mut self) -> &mut dyn Any;
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
    // fn new_steel_val(&self) -> SteelVal;
    fn display(&self) -> std::result::Result<String, std::fmt::Error> {
        Ok(format!("#<{}>", self.name().to_string()))
    }

    fn as_serializable_steelval(&mut self) -> Option<SerializableSteelVal> {
        None
    }
    // fn as_underlying_type<'a>(&'a self) -> Option<&'a Self>;
}

impl<T: Custom + 'static> CustomType for T {
    fn as_any_ref(&self) -> &dyn Any {
        self as &dyn Any
    }
    fn as_any_ref_mut(&mut self) -> &mut dyn Any {
        self as &mut dyn Any
    }
    fn display(&self) -> std::result::Result<String, std::fmt::Error> {
        if let Some(formatted) = self.fmt() {
            formatted
        } else {
            Ok(format!("#<{}>", self.name().to_string()))
        }
    }

    fn as_serializable_steelval(&mut self) -> Option<SerializableSteelVal> {
        self.into_serializable_steelval()
    }
}

impl<T: CustomType + 'static> IntoSteelVal for T {
    fn into_steelval(self) -> Result<SteelVal> {
        // Ok(self.new_steel_val())
        Ok(SteelVal::Custom(Gc::new(RefCell::new(Box::new(self)))))
    }
}

pub trait IntoSerializableSteelVal {
    fn into_serializable_steelval(val: &SteelVal) -> Result<SerializableSteelVal>;
}

impl<T: CustomType + Clone + Send + Sync + 'static> IntoSerializableSteelVal for T {
    fn into_serializable_steelval(val: &SteelVal) -> Result<SerializableSteelVal> {
        if let SteelVal::Custom(v) = val {
            // let left_type = v.borrow().as_any_ref();
            // TODO: @Matt - dylibs cause issues here, as the underlying type ids are different
            // across workspaces and builds
            let left = v.borrow().as_any_ref().downcast_ref::<T>().cloned();
            let _lifted = left.ok_or_else(|| {
                let error_message = format!(
                    "Type Mismatch: Type of SteelVal: {:?}, did not match the given type: {}",
                    val,
                    std::any::type_name::<Self>()
                );
                SteelErr::new(ErrorKind::ConversionError, error_message)
            });

            todo!()
        } else {
            let error_message = format!(
                "Type Mismatch: Type of SteelVal: {:?} did not match the given type, expecting opaque struct: {}",
                val,
                std::any::type_name::<Self>()
            );

            Err(SteelErr::new(ErrorKind::ConversionError, error_message))
        }
    }
}

// impl<'a, T: CustomType + Clone + ?Sized + 'a> FromSteelVal for &'a T {
//     fn from_steelval(val: SteelVal) -> Result<Self> {
//         if let SteelVal::Custom(v) = val {
//             let left_type = v.as_any();
//             let left: Option<T> = left_type.downcast_ref::<T>().cloned();
//             left.ok_or_else(|| {
//                 let error_message = format!(
//                     "Type Mismatch: Type of SteelVal did not match the given type: {}",
//                     std::any::type_name::<Self>()
//                 );
//                 SteelErr::new(ErrorKind::ConversionError, error_message)
//             })
//         } else {
//             let error_message = format!(
//                 "Type Mismatch: Type of SteelVal did not match the given type: {}",
//                 std::any::type_name::<Self>()
//             );

//             Err(SteelErr::new(ErrorKind::ConversionError, error_message))
//         }
//     }
// }

// TODO: Marshalling out of the type could also try to yoink from a native steel struct.
// If possible, we can try to line the constructor up with the fields
impl<T: CustomType + Clone + 'static> FromSteelVal for T {
    fn from_steelval(val: &SteelVal) -> Result<Self> {
        if let SteelVal::Custom(v) = val {
            // let left_type = v.borrow().as_any_ref();
            // TODO: @Matt - dylibs cause issues here, as the underlying type ids are different
            // across workspaces and builds
            let left = v.borrow().as_any_ref().downcast_ref::<T>().cloned();
            left.ok_or_else(|| {
                let error_message = format!(
                    "Type Mismatch: Type of SteelVal: {:?}, did not match the given type: {}",
                    val,
                    std::any::type_name::<Self>()
                );
                SteelErr::new(ErrorKind::ConversionError, error_message)
            })
        } else {
            let error_message = format!(
                "Type Mismatch: Type of SteelVal: {:?} did not match the given type, expecting opaque struct: {}",
                val,
                std::any::type_name::<Self>()
            );

            Err(SteelErr::new(ErrorKind::ConversionError, error_message))
        }
    }
}

// impl<'a, T: CustomType + Clone> FromSteelVal for &'a T {
//     fn from_steelval(val: &SteelVal) -> Result<&'a T> {
//         if let SteelVal::Custom(v) = val {
//             let left_type = v.as_any_ref();
//             let left = left_type.downcast_ref::<T>();
//             left.ok_or_else(|| {
//                 let error_message = format!(
//                     "Type Mismatch: Type of SteelVal did not match the given type: {}",
//                     std::any::type_name::<Self>()
//                 );
//                 SteelErr::new(ErrorKind::ConversionError, error_message)
//             })
//         } else {
//             let error_message = format!(
//                 "Type Mismatch: Type of SteelVal did not match the given type: {}",
//                 std::any::type_name::<Self>()
//             );

//             Err(SteelErr::new(ErrorKind::ConversionError, error_message))
//         }
//     }
// }

/// The entry point for turning values into SteelVals
/// The is implemented for most primitives and collections
/// You can also manually implement this for any type, or can optionally
/// get this implementation for a custom struct by using the custom
/// steel derive.
pub trait IntoSteelVal: Sized {
    fn into_steelval(self) -> Result<SteelVal>;
}

/// The exit point for turning SteelVals into outside world values
/// This is implement for most primitives and collections
/// You can also manually implement this for any type, or can optionally
/// get this implementation for a custom struct by using the custom
/// steel derive.
pub trait FromSteelVal: Sized {
    fn from_steelval(val: &SteelVal) -> Result<Self>;
}

pub trait PrimitiveAsRef<'a>: Sized {
    fn primitive_as_ref(val: &'a SteelVal) -> Result<Self>;
}

pub struct RestArgsIter<'a, T>(
    pub std::iter::Map<std::slice::Iter<'a, SteelVal>, fn(&'a SteelVal) -> Result<T>>,
);

impl<'a, T: PrimitiveAsRef<'a> + 'a> RestArgsIter<'a, T> {
    pub fn new(
        args: std::iter::Map<std::slice::Iter<'a, SteelVal>, fn(&'a SteelVal) -> Result<T>>,
    ) -> Self {
        RestArgsIter(args)
    }

    pub fn from_slice(args: &'a [SteelVal]) -> Result<Self> {
        Ok(RestArgsIter(args.iter().map(T::primitive_as_ref)))
    }
}

impl<'a, T> Iterator for RestArgsIter<'a, T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

pub struct RestArgs<T: FromSteelVal>(pub Vec<T>);

impl<T: FromSteelVal> RestArgs<T> {
    pub fn new(args: Vec<T>) -> Self {
        RestArgs(args)
    }

    pub fn from_slice(args: &[SteelVal]) -> Result<Self> {
        args.iter()
            .map(|x| T::from_steelval(x))
            .collect::<Result<Vec<_>>>()
            .map(RestArgs)
    }
}

impl<T: FromSteelVal> std::ops::Deref for RestArgs<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

mod private {

    use std::any::Any;

    pub trait Sealed {}

    impl<T: Any> Sealed for T {}
}

// pub trait DowncastSteelval: private::Sealed {
//     type Output;
//     fn downcast(&self) -> Result<&Self::Output>;
// }

// impl DowncastSteelval for SteelVal {
//     type Output = Box<dyn CustomType>;

//     fn downcast(&self) -> Result<&Self::Output> {
//         todo!()
//     }
// }

pub enum SRef<'b, T: ?Sized + 'b> {
    Temporary(&'b T),
    Owned(Ref<'b, T>),
    // ExistingBorrow(S),
}

impl<'b, T: ?Sized + 'b> Deref for SRef<'b, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        match self {
            SRef::Temporary(inner) => inner,
            SRef::Owned(inner) => inner,
            // SRef::ExistingBorrow(inner) =>
        }
    }
}

// Can you take a steel val and execute operations on it by reference
pub trait AsRefSteelVal: Sized {
    type Nursery: Default;

    fn as_ref<'b, 'a: 'b>(
        val: &'a SteelVal,
        _nursery: &'a mut Self::Nursery,
    ) -> Result<SRef<'b, Self>>;
}

pub trait AsSlice<T> {
    fn as_slice_repr(&self) -> &[T];
}

impl<T> AsSlice<T> for Vec<T> {
    fn as_slice_repr(&self) -> &[T] {
        self.as_slice()
    }
}

// TODO: Try to incorporate these all into one trait if possible
pub trait AsRefSteelValFromUnsized<T>: Sized {
    type Output: AsSlice<T>;

    fn as_ref_from_unsized(val: &SteelVal) -> Result<Self::Output>;
}

pub trait AsRefMutSteelVal: Sized {
    fn as_mut_ref<'b, 'a: 'b>(val: &'a SteelVal) -> Result<RefMut<'b, Self>>;
}

pub trait AsRefMutSteelValFromRef: Sized {
    fn as_mut_ref_from_ref<'a>(val: &'a SteelVal) -> crate::rvals::Result<&'a mut Self>;
}

pub trait AsRefSteelValFromRef: Sized {
    fn as_ref_from_ref<'a>(val: &'a SteelVal) -> crate::rvals::Result<&'a Self>;
}

impl AsRefSteelVal for List<SteelVal> {
    type Nursery = ();

    fn as_ref<'b, 'a: 'b>(val: &'a SteelVal, _nursery: &mut ()) -> Result<SRef<'b, Self>> {
        if let SteelVal::ListV(l) = val {
            Ok(SRef::Temporary(l))
        } else {
            stop!(TypeMismatch => "Value cannot be referenced as a list")
        }
    }
}

// impl AsRefSteelVal for FunctionSignature {
//     fn as_ref<'b, 'a: 'b>(val: &'a SteelVal) -> Result<SRef<'b, Self>> {
//         if let SteelVal::FuncV(f) = val {
//             Ok(SRef::Temporary(f))
//         } else {
//             stop!(TypeMismatch => "Value cannot be referenced as a primitive function!")
//         }
//     }
// }

impl<T: CustomType + 'static> AsRefSteelVal for T {
    type Nursery = ();

    fn as_ref<'b, 'a: 'b>(
        val: &'a SteelVal,
        _nursery: &mut Self::Nursery,
    ) -> Result<SRef<'b, Self>> {
        // todo!()

        if let SteelVal::Custom(v) = val {
            let res = Ref::map(v.borrow(), |x| x.as_any_ref());

            if res.is::<T>() {
                Ok(SRef::Owned(Ref::map(res, |x| {
                    x.downcast_ref::<T>().unwrap()
                })))
            } else {
                let error_message = format!(
                    "Type Mismatch: Type of SteelVal: {} did not match the given type: {}",
                    val,
                    std::any::type_name::<Self>()
                );
                Err(SteelErr::new(ErrorKind::ConversionError, error_message))
            }
            // res
        } else {
            let error_message = format!(
                "Type Mismatch: Type of SteelVal: {} did not match the given type: {}",
                val,
                std::any::type_name::<Self>()
            );

            Err(SteelErr::new(ErrorKind::ConversionError, error_message))
        }
    }
}

impl<T: CustomType + 'static> AsRefMutSteelVal for T {
    fn as_mut_ref<'b, 'a: 'b>(val: &'a SteelVal) -> Result<RefMut<'b, Self>> {
        // todo!()

        if let SteelVal::Custom(v) = val {
            let res = RefMut::map(v.borrow_mut(), |x| x.as_any_ref_mut());

            if res.is::<T>() {
                Ok(RefMut::map(res, |x| x.downcast_mut::<T>().unwrap()))
            } else {
                let error_message = format!(
                    "Type Mismatch: Type of SteelVal did not match the given type: {}",
                    std::any::type_name::<Self>()
                );
                Err(SteelErr::new(ErrorKind::ConversionError, error_message))
            }
            // res
        } else {
            let error_message = format!(
                "Type Mismatch: Type of SteelVal did not match the given type: {}",
                std::any::type_name::<Self>()
            );

            Err(SteelErr::new(ErrorKind::ConversionError, error_message))
        }
    }
}

// ListV(l) => {
//     // Rooted - things operate as normal
//     if self.qq_depth == 0 {
//         let maybe_special_form = l.first().and_then(|x| x.as_string());

//         match maybe_special_form {
//             Some(x) if x.as_str() == "quote" => {
//                 if self.quoted {
//                     let items: std::result::Result<Vec<ExprKind>, &'static str> =
//                         l.iter().map(|x| self.visit(x)).collect();

//                     return Ok(ExprKind::List(List::new(items?)));
//                 }

//                 self.quoted = true;

//                 let return_value = l
//                     .into_iter()
//                     .map(|x| self.visit(x))
//                     .collect::<std::result::Result<Vec<_>, _>>()?
//                     .try_into()
//                     .map_err(|_| {
//                         "parse error! If you're running into this, please file an issue"
//                     });

//                 self.quoted = false;

//                 return return_value;
//             } // "quasiquote" => {
//             //     self.qq_depth += 1;
//             // }
//             _ => {}
//         }
//     }

//     l.into_iter()
//         .map(|x| self.visit(x))
//         .collect::<std::result::Result<Vec<_>, _>>()?
//         .try_into()
//         .map_err(|_| "If you're running into this, please file an issue")

//     // If we're not quoted, we need to just return this pushed down to an ast
//     // let items: std::result::Result<Vec<ExprKind>, &'static str> =
//     // l.iter().map(|x| self.visit(x)).collect();

//     // Ok(ExprKind::List(List::new(items?)))
// }

impl ast::TryFromSteelValVisitorForExprKind {
    pub fn visit_syntax_object(&mut self, value: &Syntax) -> Result<ExprKind> {
        let span = value.span;
        // let source = self.source.clone();
        match &value.syntax {
            // Mutual recursion case
            SyntaxObject(s) => self.visit_syntax_object(s),
            BoolV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::BooleanLiteral(*x),
                span,
            )))),
            NumV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::NumberLiteral(*x),
                span,
            )))),
            IntV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::IntegerLiteral(MaybeBigInt::Small(*x)),
                span,
            )))),
            VectorV(lst) => {
                let items: Result<Vec<ExprKind>> = lst.iter().map(|x| self.visit(x)).collect();
                Ok(ExprKind::List(crate::parser::ast::List::new(items?)))
            }
            StringV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::StringLiteral(x.to_string()),
                span,
            )))),
            // LambdaV(_) => Err("Can't convert from Lambda to expression!"),
            // MacroV(_) => Err("Can't convert from Macro to expression!"),
            SymbolV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::Identifier(x.as_str().into()),
                span,
            )))),

            ListV(l) => {
                // dbg!(&self);
                // dbg!(&l);

                // Rooted - things operate as normal
                if self.qq_depth == 0 {
                    let maybe_special_form = l.first().and_then(|x| {
                        x.as_symbol()
                            .or_else(|| x.as_syntax_object().and_then(|x| x.syntax.as_symbol()))
                    });

                    // dbg!(&maybe_special_form);

                    match maybe_special_form {
                        Some(x) if x.as_str() == "quote" => {
                            if self.quoted {
                                let items: std::result::Result<Vec<ExprKind>, _> =
                                    l.iter().map(|x| self.visit(x)).collect();

                                return Ok(ExprKind::List(ast::List::new(items?)));
                            }

                            self.quoted = true;

                            let return_value = l
                                .into_iter()
                                .map(|x| self.visit(x))
                                .collect::<std::result::Result<Vec<_>, _>>()?
                                .try_into()?;

                            self.quoted = false;

                            return Ok(return_value);
                        } // "quasiquote" => {
                        //     self.qq_depth += 1;
                        // }
                        // None => {
                        // return Ok(ExprKind::empty());
                        // }
                        _ => {}
                    }
                }

                Ok(l.into_iter()
                    .map(|x| self.visit(x))
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .try_into()?)
            }

            CharV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::CharacterLiteral(*x),
                span,
            )))),
            _ => stop!(ConversionError => "unable to convert {:?} to expression", &value.syntax),
        }
    }
}

// TODO: Replace this with RawSyntaxObject<SteelVal>

#[derive(Debug, Clone)]
pub struct Syntax {
    raw: Option<SteelVal>,
    pub(crate) syntax: SteelVal,
    span: Span,
}

impl Syntax {
    pub fn new(syntax: SteelVal, span: Span) -> Syntax {
        // dbg!(&syntax);

        Self {
            raw: None,
            syntax,
            span,
        }
    }

    pub fn proto(raw: SteelVal, syntax: SteelVal, span: Span) -> Syntax {
        Self {
            raw: Some(raw),
            syntax,
            span,
        }
    }

    pub fn syntax_e(&self) -> SteelVal {
        self.syntax.clone()
    }

    pub fn new_with_source(syntax: SteelVal, span: Span) -> Syntax {
        Self {
            raw: None,
            syntax,
            span,
        }
    }

    pub fn syntax_loc(&self) -> Span {
        self.span
    }

    pub fn syntax_datum(&self) -> SteelVal {
        self.raw.clone().unwrap()
    }

    pub(crate) fn steelval_to_exprkind(value: &SteelVal) -> Result<ExprKind> {
        match value {
            // Mutual recursion case
            SyntaxObject(s) => s.to_exprkind(),
            BoolV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::default(
                TokenType::BooleanLiteral(*x),
            )))),
            NumV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::default(
                TokenType::NumberLiteral(*x),
            )))),
            IntV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::default(
                TokenType::IntegerLiteral(MaybeBigInt::Small(*x)),
            )))),
            VectorV(lst) => {
                let items: Result<Vec<ExprKind>> =
                    lst.iter().map(Self::steelval_to_exprkind).collect();
                Ok(ExprKind::List(crate::parser::ast::List::new(items?)))
            }
            StringV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::default(
                TokenType::StringLiteral(x.to_string()),
            )))),
            // LambdaV(_) => Err("Can't convert from Lambda to expression!"),
            // MacroV(_) => Err("Can't convert from Macro to expression!"),
            SymbolV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::default(
                TokenType::Identifier(x.as_str().into()),
            )))),
            ListV(l) => {
                let items: Result<Vec<ExprKind>> =
                    l.iter().map(Self::steelval_to_exprkind).collect();

                Ok(ExprKind::List(crate::parser::ast::List::new(items?)))
            }
            CharV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::default(
                TokenType::CharacterLiteral(*x),
            )))),
            _ => stop!(ConversionError => "unable to convert {:?} to expression", value),
        }
    }

    // TODO: match on self.syntax. If its itself a syntax object, then just recur on that until we bottom out
    // Otherwise, reconstruct the ExprKind and replace the span and source information into the representation
    pub fn to_exprkind(&self) -> Result<ExprKind> {
        let span = self.span;
        // let source = self.source.clone();
        match &self.syntax {
            // Mutual recursion case
            SyntaxObject(s) => s.to_exprkind(),
            BoolV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::BooleanLiteral(*x),
                span,
            )))),
            NumV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::NumberLiteral(*x),
                span,
            )))),
            IntV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::IntegerLiteral(MaybeBigInt::Small(*x)),
                span,
            )))),
            VectorV(lst) => {
                let items: Result<Vec<ExprKind>> =
                    lst.iter().map(Self::steelval_to_exprkind).collect();
                Ok(ExprKind::List(crate::parser::ast::List::new(items?)))
            }
            StringV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::StringLiteral(x.to_string()),
                span,
            )))),
            // LambdaV(_) => Err("Can't convert from Lambda to expression!"),
            // MacroV(_) => Err("Can't convert from Macro to expression!"),
            SymbolV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::Identifier(x.as_str().into()),
                span,
            )))),
            ListV(l) => {
                let items: Result<Vec<ExprKind>> =
                    l.iter().map(Self::steelval_to_exprkind).collect();

                Ok(ExprKind::List(crate::parser::ast::List::new(items?)))
            }
            CharV(x) => Ok(ExprKind::Atom(Atom::new(SyntaxObject::new(
                TokenType::CharacterLiteral(*x),
                span,
            )))),
            _ => stop!(ConversionError => "unable to convert {:?} to expression", &self.syntax),
        }
    }
}

impl IntoSteelVal for Syntax {
    fn into_steelval(self) -> Result<SteelVal> {
        Ok(SteelVal::SyntaxObject(Gc::new(self)))
    }
}

impl AsRefSteelVal for Syntax {
    type Nursery = ();

    fn as_ref<'b, 'a: 'b>(
        val: &'a SteelVal,
        _nursery: &'a mut Self::Nursery,
    ) -> Result<SRef<'b, Self>> {
        if let SteelVal::SyntaxObject(s) = val {
            Ok(SRef::Temporary(s))
        } else {
            stop!(TypeMismatch => "Value cannot be referenced as a syntax object")
        }
    }
}

impl From<Syntax> for SteelVal {
    fn from(val: Syntax) -> Self {
        SteelVal::SyntaxObject(Gc::new(val))
    }
}

// Values which can be sent to another thread.
// If it cannot be sent to another thread, then we'll error out on conversion.
// TODO: Add boxed dyn functions to this.
// #[derive(PartialEq)]
pub enum SerializableSteelVal {
    Closure(crate::values::functions::SerializedLambda),
    BoolV(bool),
    NumV(f64),
    IntV(isize),
    CharV(char),
    Void,
    StringV(String),
    FuncV(FunctionSignature),
    HashMapV(Vec<(SerializableSteelVal, SerializableSteelVal)>),
    // If the value
    VectorV(Vec<SerializableSteelVal>),
    BoxedDynFunction(BoxedDynFunction),
    BuiltIn(BuiltInSignature),
    SymbolV(String),
    Custom(Box<dyn CustomType + Send>), // Custom()
}

// Once crossed over the line, convert BACK into a SteelVal
// This should be infallible.
pub fn from_serializable_value(val: SerializableSteelVal) -> SteelVal {
    match val {
        SerializableSteelVal::Closure(c) => SteelVal::Closure(Gc::new(c.into())),
        SerializableSteelVal::BoolV(b) => SteelVal::BoolV(b),
        SerializableSteelVal::NumV(n) => SteelVal::NumV(n),
        SerializableSteelVal::IntV(i) => SteelVal::IntV(i),
        SerializableSteelVal::CharV(c) => SteelVal::CharV(c),
        SerializableSteelVal::Void => SteelVal::Void,
        SerializableSteelVal::StringV(s) => SteelVal::StringV(s.into()),
        SerializableSteelVal::FuncV(f) => SteelVal::FuncV(f),
        SerializableSteelVal::HashMapV(h) => SteelVal::HashMapV(Gc::new(
            h.into_iter()
                .map(|(k, v)| (from_serializable_value(k), from_serializable_value(v)))
                .collect(),
        )),
        SerializableSteelVal::VectorV(v) => {
            SteelVal::ListV(v.into_iter().map(from_serializable_value).collect())
        }
        SerializableSteelVal::BoxedDynFunction(f) => SteelVal::BoxedFunction(Rc::new(f)),
        SerializableSteelVal::BuiltIn(f) => SteelVal::BuiltIn(f),
        SerializableSteelVal::SymbolV(s) => SteelVal::SymbolV(s.into()),
        SerializableSteelVal::Custom(b) => SteelVal::Custom(Gc::new(RefCell::new(b))),
    }
}

pub fn into_serializable_value(val: SteelVal) -> Result<SerializableSteelVal> {
    match val {
        SteelVal::Closure(c) => Ok(SerializableSteelVal::Closure(c.unwrap().try_into()?)),
        SteelVal::BoolV(b) => Ok(SerializableSteelVal::BoolV(b)),
        SteelVal::NumV(n) => Ok(SerializableSteelVal::NumV(n)),
        SteelVal::IntV(n) => Ok(SerializableSteelVal::IntV(n)),
        SteelVal::CharV(c) => Ok(SerializableSteelVal::CharV(c)),
        SteelVal::Void => Ok(SerializableSteelVal::Void),
        SteelVal::StringV(s) => Ok(SerializableSteelVal::StringV(s.to_string())),
        SteelVal::FuncV(f) => Ok(SerializableSteelVal::FuncV(f)),
        SteelVal::ListV(l) => Ok(SerializableSteelVal::VectorV(
            l.into_iter()
                .map(into_serializable_value)
                .collect::<Result<_>>()?,
        )),
        SteelVal::BoxedFunction(f) => Ok(SerializableSteelVal::BoxedDynFunction((*f).clone())),
        SteelVal::BuiltIn(f) => Ok(SerializableSteelVal::BuiltIn(f)),
        SteelVal::SymbolV(s) => Ok(SerializableSteelVal::SymbolV(s.to_string())),

        SteelVal::HashMapV(v) => Ok(SerializableSteelVal::HashMapV(
            v.unwrap()
                .into_iter()
                .map(|(k, v)| {
                    let kprime = into_serializable_value(k)?;
                    let vprime = into_serializable_value(v)?;

                    Ok((kprime, vprime))
                })
                .collect::<Result<_>>()?,
        )),

        SteelVal::Custom(c) => {
            if let Some(output) = c.borrow_mut().as_serializable_steelval() {
                Ok(output)
            } else {
                stop!(Generic => "Custom type not allowed to be moved across threads!")
            }
        }
        illegal => stop!(Generic => "Type not allowed to be moved across threads!: {}", illegal),
    }
}

/// A value as represented in the runtime.
#[derive(Clone)]
pub enum SteelVal {
    /// Represents a bytecode closure
    Closure(Gc<ByteCodeLambda>),
    /// Represents a boolean value
    BoolV(bool),
    /// Represents a number, currently only f64 numbers are supported
    NumV(f64),
    /// Represents an integer
    IntV(isize),
    /// Represents a character type
    CharV(char),
    /// Vectors are represented as `im_rc::Vector`'s, which are immutable
    /// data structures
    VectorV(Gc<Vector<SteelVal>>),
    /// Void return value
    Void,
    /// Represents strings
    StringV(SteelString),
    /// Represents built in rust functions
    FuncV(FunctionSignature),
    /// Represents a symbol, internally represented as `String`s
    SymbolV(SteelString),
    /// Container for a type that implements the `Custom Type` trait. (trait object)
    Custom(Gc<RefCell<Box<dyn CustomType>>>),
    // Embedded HashMap
    HashMapV(Gc<HashMap<SteelVal, SteelVal>>),
    // Embedded HashSet
    HashSetV(Gc<im_rc::HashSet<SteelVal>>),
    /// Represents a scheme-only struct
    // StructV(Gc<SteelStruct>),
    /// Alternative implementation of a scheme-only struct
    CustomStruct(Gc<RefCell<UserDefinedStruct>>),
    // Represents a special rust closure
    // StructClosureV(Box<SteelStruct>, StructClosureSignature),
    // StructClosureV(Box<StructClosure>),
    /// Represents a port object
    PortV(Gc<SteelPort>),
    /// Generic iterator wrapper
    IterV(Gc<Transducer>),
    /// Reducers
    ReducerV(Gc<Reducer>),
    // Reducer(Reducer)
    // Generic IntoIter wrapper
    // Promise(Gc<SteelVal>),
    /// Async Function wrapper
    FutureFunc(BoxedAsyncFunctionSignature),
    // Boxed Future Result
    FutureV(Gc<FutureResult>),

    StreamV(Gc<LazyStream>),

    /// Contract
    Contract(Gc<ContractType>),
    /// Contracted Function
    ContractedFunction(Gc<ContractedFunction>),
    /// Custom closure
    BoxedFunction(Rc<BoxedDynFunction>),
    // Continuation
    ContinuationFunction(Gc<Continuation>),
    // Function Pointer
    // #[cfg(feature = "jit")]
    // CompiledFunction(Box<JitFunctionPointer>),
    // List
    ListV(List<SteelVal>),
    // Mutable functions
    MutFunc(MutFunctionSignature),
    // Built in functions
    BuiltIn(BuiltInSignature),
    // Mutable vector
    MutableVector(Gc<RefCell<Vec<SteelVal>>>),
    // This should delegate to the underlying iterator - can allow for faster raw iteration if possible
    // Should allow for polling just a raw "next" on underlying elements
    BoxedIterator(Gc<RefCell<BuiltInDataStructureIterator>>),

    SyntaxObject(Gc<Syntax>),

    // Mutable storage, with Gc backing
    // Boxed(HeapRef),
    Boxed(Gc<RefCell<SteelVal>>),

    // TODO: This itself, needs to be boxed unfortunately.
    Reference(Rc<OpaqueReference<'static>>),

    BigNum(Gc<num::BigInt>),
}

// TODO: Consider unboxed value types, for optimized usages when compiling segments of code.
// If we can infer the types from the concrete functions used, we don't need to have unboxed values -> We also
// can use concrete forms of the underlying functions as well.
// #[derive(Clone)]
// pub enum UnboxedSteelVal {
//     /// Represents a boolean value
//     BoolV(bool),
//     /// Represents a number, currently only f64 numbers are supported
//     NumV(f64),
//     /// Represents an integer
//     IntV(isize),
//     /// Represents a character type
//     CharV(char),
//     /// Vectors are represented as `im_rc::Vector`'s, which are immutable
//     /// data structures
//     VectorV(Vector<SteelVal>),
//     /// Void return value
//     Void,
//     /// Represents strings
//     StringV(SteelString),
//     /// Represents built in rust functions
//     FuncV(FunctionSignature),
//     /// Represents a symbol, internally represented as `String`s
//     SymbolV(SteelString),
//     /// Container for a type that implements the `Custom Type` trait. (trait object)
//     Custom(Gc<RefCell<Box<dyn CustomType>>>),
//     // Embedded HashMap
//     HashMapV(HashMap<SteelVal, SteelVal>),
//     // Embedded HashSet
//     HashSetV(HashSet<SteelVal>),
//     /// Represents a scheme-only struct
//     // StructV(Gc<SteelStruct>),
//     /// Alternative implementation of a scheme-only struct
//     CustomStruct(Gc<RefCell<UserDefinedStruct>>),
//     // Represents a special rust closure
//     // StructClosureV(Box<SteelStruct>, StructClosureSignature),
//     // StructClosureV(Box<StructClosure>),
//     /// Represents a port object
//     PortV(SteelPort),
//     /// Represents a bytecode closure
//     Closure(Gc<ByteCodeLambda>),
//     /// Generic iterator wrapper
//     IterV(Gc<Transducer>),
//     /// Reducers
//     ReducerV(Gc<Reducer>),
//     // Reducer(Reducer)
//     // Generic IntoIter wrapper
//     // Promise(Gc<SteelVal>),
//     /// Async Function wrapper
//     FutureFunc(BoxedAsyncFunctionSignature),
//     // Boxed Future Result
//     FutureV(Gc<FutureResult>),

//     StreamV(Gc<LazyStream>),
//     // Break the cycle somehow
//     // EvaluationEnv(Weak<RefCell<Env>>),
//     /// Contract
//     Contract(Gc<ContractType>),
//     /// Contracted Function
//     ContractedFunction(Gc<ContractedFunction>),
//     /// Custom closure
//     BoxedFunction(BoxedFunctionSignature),
//     // Continuation
//     ContinuationFunction(Gc<Continuation>),
//     // List
//     ListV(List<SteelVal>),
//     // Mutable functions
//     MutFunc(MutFunctionSignature),
//     // Built in functions
//     BuiltIn(BuiltInSignature),
//     // Mutable vector
//     MutableVector(Gc<RefCell<Vec<SteelVal>>>),
//     // This should delegate to the underlying iterator - can allow for faster raw iteration if possible
//     // Should allow for polling just a raw "next" on underlying elements
//     BoxedIterator(Gc<RefCell<BuiltInDataStructureIterator>>),

//     SyntaxObject(Gc<Syntax>),

//     // Mutable storage, with Gc backing
//     Boxed(HeapRef),
// }

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct SteelString(Rc<String>);

impl Deref for SteelString {
    type Target = Rc<String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<&str> for SteelString {
    fn from(val: &str) -> Self {
        SteelString(Rc::new(val.to_string()))
    }
}

impl From<&String> for SteelString {
    fn from(val: &String) -> Self {
        SteelString(Rc::new(val.to_owned()))
    }
}

impl From<String> for SteelString {
    fn from(val: String) -> Self {
        SteelString(Rc::new(val))
    }
}

impl From<Rc<String>> for SteelString {
    fn from(val: Rc<String>) -> Self {
        SteelString(val)
    }
}

impl From<SteelString> for Rc<String> {
    fn from(value: SteelString) -> Self {
        value.0
    }
}

impl std::fmt::Display for SteelString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Debug for SteelString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[test]
fn check_size_of_steelval() {
    assert_eq!(std::mem::size_of::<SteelVal>(), 16);
}

pub struct Chunks {
    remaining: IntoIter<char>,
}

impl Chunks {
    fn new(s: SteelString) -> Self {
        Chunks {
            remaining: s.chars().collect::<Vec<_>>().into_iter(),
        }
    }
}

pub enum BuiltInDataStructureIterator {
    List(im_lists::list::ConsumingIter<SteelVal, im_lists::shared::RcPointer, 256, 1>),
    Vector(im_rc::vector::ConsumingIter<SteelVal>),
    Set(im_rc::hashset::ConsumingIter<SteelVal>),
    Map(im_rc::hashmap::ConsumingIter<(SteelVal, SteelVal)>),
    String(Chunks),
    Opaque(Box<dyn Iterator<Item = SteelVal>>),
}

impl BuiltInDataStructureIterator {
    pub fn into_boxed_iterator(self) -> SteelVal {
        SteelVal::BoxedIterator(Gc::new(RefCell::new(self)))
    }
}

impl BuiltInDataStructureIterator {
    pub fn from_iterator<T: IntoSteelVal, S: IntoIterator<Item = T> + 'static>(value: S) -> Self {
        Self::Opaque(Box::new(
            value
                .into_iter()
                .map(|x| x.into_steelval().expect("This shouldn't fail!")),
        ))
    }
}

impl Iterator for BuiltInDataStructureIterator {
    type Item = SteelVal;

    fn next(&mut self) -> Option<SteelVal> {
        match self {
            Self::List(l) => l.next(),
            Self::Vector(v) => v.next(),
            Self::String(s) => s.remaining.next().map(SteelVal::CharV),
            Self::Set(s) => s.next(),
            Self::Map(s) => s.next().map(|x| SteelVal::ListV(im_lists::list![x.0, x.1])),
            Self::Opaque(s) => s.next(),
        }
    }
}

pub fn value_into_iterator(val: SteelVal) -> SteelVal {
    match val {
        SteelVal::ListV(l) => BuiltInDataStructureIterator::List(l.into_iter()),
        SteelVal::VectorV(v) => BuiltInDataStructureIterator::Vector((*v).clone().into_iter()),
        SteelVal::StringV(s) => BuiltInDataStructureIterator::String(Chunks::new(s)),
        SteelVal::HashSetV(s) => BuiltInDataStructureIterator::Set((*s).clone().into_iter()),
        SteelVal::HashMapV(m) => BuiltInDataStructureIterator::Map((*m).clone().into_iter()),
        _ => panic!("Haven't handled this case yet"),
    }
    .into_boxed_iterator()
}

pub fn iterator_next(args: &[SteelVal]) -> Result<SteelVal> {
    match &args[0] {
        SteelVal::BoxedIterator(b) => match b.borrow_mut().next() {
            Some(v) => Ok(v),
            None => Ok(SteelVal::Void),
        },
        _ => stop!(TypeMismatch => "Unexpected argument"),
    }
}

impl SteelVal {
    pub fn boxed(value: SteelVal) -> SteelVal {
        SteelVal::Boxed(Gc::new(RefCell::new(value)))
    }

    pub(crate) fn ptr_eq(&self, other: &SteelVal) -> bool {
        match (self, other) {
            (BoolV(l), BoolV(r)) => l == r,
            (VectorV(l), VectorV(r)) => Gc::ptr_eq(l, r),
            (Void, Void) => true,
            (StringV(l), StringV(r)) => Rc::ptr_eq(l, r),
            (FuncV(l), FuncV(r)) => *l as usize == *r as usize,
            (SymbolV(l), SymbolV(r)) => Rc::ptr_eq(l, r),
            (SteelVal::Custom(l), SteelVal::Custom(r)) => Gc::ptr_eq(l, r),
            (HashMapV(l), HashMapV(r)) => Gc::ptr_eq(l, r),
            (HashSetV(l), HashSetV(r)) => Gc::ptr_eq(l, r),
            (PortV(l), PortV(r)) => Gc::ptr_eq(l, r),
            (Closure(l), Closure(r)) => Gc::ptr_eq(l, r),
            (IterV(l), IterV(r)) => Gc::ptr_eq(l, r),
            (ReducerV(l), ReducerV(r)) => Gc::ptr_eq(l, r),
            #[allow(clippy::vtable_address_comparisons)]
            (FutureFunc(l), FutureFunc(r)) => Rc::ptr_eq(l, r),
            (FutureV(l), FutureV(r)) => Gc::ptr_eq(l, r),
            (StreamV(l), StreamV(r)) => Gc::ptr_eq(l, r),
            (Contract(l), Contract(r)) => Gc::ptr_eq(l, r),
            (SteelVal::ContractedFunction(l), SteelVal::ContractedFunction(r)) => Gc::ptr_eq(l, r),
            (BoxedFunction(l), BoxedFunction(r)) => Rc::ptr_eq(l, r),
            (ContinuationFunction(l), ContinuationFunction(r)) => Gc::ptr_eq(l, r),
            // (CompiledFunction(_), CompiledFunction(_)) => todo!(),
            (ListV(l), ListV(r)) => l.ptr_eq(r),
            (MutFunc(l), MutFunc(r)) => *l as usize == *r as usize,
            (BuiltIn(l), BuiltIn(r)) => *l as usize == *r as usize,
            (MutableVector(l), MutableVector(r)) => Gc::ptr_eq(l, r),
            (_, _) => false,
        }
    }
}

// TODO come back to this for the constant map

// impl Serialize for SteelVal {
//     fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
//     where
//         S: Serializer,
//     {
//         match self {
//             SteelVal::BoolV(b) => serializer.serialize_newtype_variant("SteelVal", 0, "BoolV", b),
//             SteelVal::NumV(n) => serializer.serialize_newtype_variant("SteelVal", 1, "NumV", n),
//             SteelVal::IntV(n) => serializer.serialize_newtype_variant("SteelVal", 2, "IntV", n),
//             SteelVal::CharV(c) => serializer.serialize_newtype_variant("SteelVal", 3, "CharV", c),
//             SteelVal::StringV(s) => {
//                 serializer.serialize_newtype_variant("SteelVal", 7, "StringV", s)
//             }
//             SteelVal::Pair(car, cdr) => {
//                 let mut state = serializer.serialize_tuple_variant("SteelVal", 4, "Pair", 2)?;
//                 state.serialize_field(car)?;
//                 state.serialize_field(cdr)?;
//                 state.end()
//             }
//             _ => panic!("Cannot serialize enum variant: {}", self),
//         }
//     }
// }

impl Hash for SteelVal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            BoolV(b) => b.hash(state),
            NumV(n) => n.to_string().hash(state),
            IntV(i) => i.hash(state),
            CharV(c) => c.hash(state),
            ListV(l) => l.hash(state),
            CustomStruct(s) => s.borrow().hash(state),
            // Pair(cell) => {
            //     cell.hash(state);
            // }
            VectorV(v) => v.hash(state),
            v @ Void => v.hash(state),
            StringV(s) => s.hash(state),
            FuncV(s) => (*s as *const FunctionSignature).hash(state),
            // LambdaV(_) => unimplemented!(),
            // MacroV(_) => unimplemented!(),
            SymbolV(sym) => {
                "symbol".hash(state);
                sym.hash(state);
                // format!("symbol: {}")
            }
            Custom(_) => unimplemented!(),
            // StructClosureV(_) => unimplemented!(),
            PortV(_) => unimplemented!(),
            Closure(b) => b.hash(state),
            HashMapV(hm) => hm.hash(state),
            IterV(s) => s.hash(state),
            HashSetV(hs) => hs.hash(state),
            _ => {
                println!("Trying to hash: {self:?}");
                unimplemented!()
            } // Promise(_) => unimplemented!(),
        }
    }
}

impl SteelVal {
    #[inline(always)]
    pub fn is_truthy(&self) -> bool {
        match &self {
            SteelVal::BoolV(false) => false,
            SteelVal::Void => false,
            SteelVal::ListV(v) => !v.is_empty(),
            _ => true,
        }
    }

    #[inline(always)]
    pub fn is_falsey(&self) -> bool {
        match &self {
            SteelVal::BoolV(false) => true,
            SteelVal::Void => true,
            SteelVal::ListV(v) => v.is_empty(),
            _ => false,
        }
    }

    #[inline(always)]
    pub fn is_future(&self) -> bool {
        matches!(self, SteelVal::FutureV(_))
    }

    pub fn is_hashable(&self) -> bool {
        matches!(
            self,
            BoolV(_)
                | IntV(_)
                | CharV(_)
                // | Pair(_)
                | VectorV(_)
                | StringV(_)
                | SymbolV(_)
                | HashMapV(_)
                | Closure(_)
                | ListV(_)
                | FuncV(_)
                | CustomStruct(_)
        )
    }

    pub fn is_function(&self) -> bool {
        matches!(
            self,
            BoxedFunction(_)
                | Closure(_)
                | FuncV(_)
                | ContractedFunction(_)
                | BuiltIn(_)
                | MutFunc(_)
        )
    }

    pub fn is_contract(&self) -> bool {
        matches!(self, Contract(_))
    }

    pub fn empty_hashmap() -> SteelVal {
        SteelVal::HashMapV(Gc::new(HashMap::new()))
    }
}

impl SteelVal {
    // pub fn res_iterator

    pub fn list_or_else<E, F: FnOnce() -> E>(
        &self,
        err: F,
    ) -> std::result::Result<&List<SteelVal>, E> {
        match self {
            Self::ListV(v) => Ok(v),
            _ => Err(err()),
        }
    }

    pub fn list(&self) -> Option<&List<SteelVal>> {
        match self {
            Self::ListV(l) => Some(l),
            _ => None,
        }
    }

    pub fn bool_or_else<E, F: FnOnce() -> E>(&self, err: F) -> std::result::Result<bool, E> {
        match self {
            Self::BoolV(v) => Ok(*v),
            _ => Err(err()),
        }
    }

    pub fn int_or_else<E, F: FnOnce() -> E>(&self, err: F) -> std::result::Result<isize, E> {
        match self {
            Self::IntV(v) => Ok(*v),
            _ => Err(err()),
        }
    }

    pub fn num_or_else<E, F: FnOnce() -> E>(&self, err: F) -> std::result::Result<f64, E> {
        match self {
            Self::NumV(v) => Ok(*v),
            _ => Err(err()),
        }
    }

    pub fn char_or_else<E, F: FnOnce() -> E>(&self, err: F) -> std::result::Result<char, E> {
        match self {
            Self::CharV(v) => Ok(*v),
            _ => Err(err()),
        }
    }

    /// Vector does copy on the value to return
    pub fn vector_or_else<E, F: FnOnce() -> E>(
        &self,
        err: F,
    ) -> std::result::Result<Vector<SteelVal>, E> {
        match self {
            Self::VectorV(v) => Ok(v.unwrap()),
            _ => Err(err()),
        }
    }

    pub fn void_or_else<E, F: FnOnce() -> E>(&self, err: F) -> std::result::Result<(), E> {
        match self {
            Self::Void => Ok(()),
            _ => Err(err()),
        }
    }

    pub fn string_or_else<E, F: FnOnce() -> E>(&self, err: F) -> std::result::Result<&str, E> {
        match self {
            Self::StringV(v) => Ok(v),
            _ => Err(err()),
        }
    }

    pub fn func_or_else<E, F: FnOnce() -> E>(
        &self,
        err: F,
    ) -> std::result::Result<&FunctionSignature, E> {
        match self {
            Self::FuncV(v) => Ok(v),
            _ => Err(err()),
        }
    }

    pub fn boxed_func_or_else<E, F: FnOnce() -> E>(
        &self,
        err: F,
    ) -> std::result::Result<&BoxedDynFunction, E> {
        match self {
            Self::BoxedFunction(v) => Ok(v),
            _ => Err(err()),
        }
    }

    pub fn contract_or_else<E, F: FnOnce() -> E>(
        &self,
        err: F,
    ) -> std::result::Result<Gc<ContractType>, E> {
        match self {
            Self::Contract(c) => Ok(c.clone()),
            _ => Err(err()),
        }
    }

    pub fn closure_or_else<E, F: FnOnce() -> E>(
        &self,
        err: F,
    ) -> std::result::Result<Gc<ByteCodeLambda>, E> {
        match self {
            Self::Closure(c) => Ok(c.clone()),
            _ => Err(err()),
        }
    }

    pub fn symbol_or_else<E, F: FnOnce() -> E>(&self, err: F) -> std::result::Result<&str, E> {
        match self {
            Self::SymbolV(v) => Ok(v),
            _ => Err(err()),
        }
    }

    pub fn clone_symbol_or_else<E, F: FnOnce() -> E>(
        &self,
        err: F,
    ) -> std::result::Result<String, E> {
        match self {
            Self::SymbolV(v) => Ok(v.to_string()),
            _ => Err(err()),
        }
    }

    pub fn as_isize(&self) -> Option<isize> {
        match self {
            Self::IntV(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_usize(&self) -> Option<usize> {
        self.as_isize()
            .and_then(|x| if x >= 0 { Some(x as usize) } else { None })
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::BoolV(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_future(&self) -> Option<Shared<BoxedFutureResult>> {
        match self {
            Self::FutureV(v) => Some(v.clone().unwrap().into_shared()),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&SteelString> {
        match self {
            Self::StringV(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_symbol(&self) -> Option<&SteelString> {
        match self {
            Self::SymbolV(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_syntax_object(&self) -> Option<&Syntax> {
        match self {
            Self::SyntaxObject(s) => Some(s),
            _ => None,
        }
    }

    // pub fn custom_or_else<E, F: FnOnce() -> E>(
    //     &self,
    //     err: F,
    // ) -> std::result::Result<&Box<dyn CustomType>, E> {
    //     match self {
    //         Self::Custom(v) => Ok(&v),
    //         _ => Err(err()),
    //     }
    // }

    // pub fn struct_or_else<E, F: FnOnce() -> E>(
    //     &self,
    //     err: F,
    // ) -> std::result::Result<&SteelStruct, E> {
    //     match self {
    //         Self::StructV(v) => Ok(v),
    //         _ => Err(err()),
    //     }
    // }

    pub fn closure_arity(&self) -> Option<usize> {
        if let SteelVal::Closure(c) = self {
            Some(c.arity())
        } else {
            None
        }
    }
}

impl SteelVal {
    pub const INT_ZERO: SteelVal = SteelVal::IntV(0);
    pub const INT_ONE: SteelVal = SteelVal::IntV(1);
    pub const INT_TWO: SteelVal = SteelVal::IntV(2);
}

impl Eq for SteelVal {}

// TODO add tests
impl PartialEq for SteelVal {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Void, Void) => true,
            (BoolV(l), BoolV(r)) => l == r,
            (BigNum(l), BigNum(r)) => l == r,
            // (NumV(l), NumV(r)) => l == r,
            (IntV(l), IntV(r)) => l == r,
            // (NumV(l), IntV(r)) => *l == *r as f64,
            // (IntV(l), NumV(r)) => *l as f64 == *r,
            (StringV(l), StringV(r)) => l == r,
            (VectorV(l), VectorV(r)) => l == r,
            (SymbolV(l), SymbolV(r)) => l == r,
            (CharV(l), CharV(r)) => l == r,
            // (Pair(_), Pair(_)) => collect_pair_into_vector(self) == collect_pair_into_vector(other),
            (HashSetV(l), HashSetV(r)) => l == r,
            (HashMapV(l), HashMapV(r)) => l == r,
            (Closure(l), Closure(r)) => l == r,
            (ContractedFunction(l), ContractedFunction(r)) => l == r,
            (Contract(l), Contract(r)) => l == r,
            (IterV(l), IterV(r)) => l == r,
            (ListV(l), ListV(r)) => l == r,
            (CustomStruct(l), CustomStruct(r)) => l == r,
            (FuncV(l), FuncV(r)) => *l as usize == *r as usize,
            //TODO
            (_, _) => false, // (l, r) => {
                             //     let left = unwrap!(l, usize);
                             //     let right = unwrap!(r, usize);
                             //     match (left, right) {
                             //         (Ok(l), Ok(r)) => l == r,
                             //         (_, _) => false,
                             //     }
                             // }
        }
    }
}

// TODO add tests
impl PartialOrd for SteelVal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (NumV(n), NumV(o)) => n.partial_cmp(o),
            (StringV(s), StringV(o)) => s.partial_cmp(o),
            (CharV(l), CharV(r)) => l.partial_cmp(r),
            (IntV(l), IntV(r)) => l.partial_cmp(r),
            _ => None, // unimplemented for other types
        }
    }
}

impl fmt::Display for SteelVal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // at the top level, print a ' if we are
        // trying to print a symbol or list
        match self {
            SymbolV(_) | ListV(_) => write!(f, "'")?,
            VectorV(_) => write!(f, "'#")?,
            _ => (),
        };

        CycleDetector::detect_and_display_cycles(self, f)

        // display_helper(self, f)
    }
}

impl fmt::Debug for SteelVal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // at the top level, print a ' if we are
        // trying to print a symbol or list
        match self {
            SymbolV(_) | ListV(_) => write!(f, "'")?,
            VectorV(_) => write!(f, "'#")?,
            _ => (),
        };
        // display_helper(self, f)

        CycleDetector::detect_and_display_cycles(self, f)
    }
}

#[cfg(test)]
mod or_else_tests {

    use super::*;
    use im_rc::vector;

    #[test]
    fn bool_or_else_test_good() {
        let input = SteelVal::BoolV(true);
        assert_eq!(input.bool_or_else(throw!(Generic => "test")).unwrap(), true);
    }

    #[test]
    fn bool_or_else_test_bad() {
        let input = SteelVal::CharV('f');
        assert!(input.bool_or_else(throw!(Generic => "test")).is_err());
    }

    #[test]
    fn num_or_else_test_good() {
        let input = SteelVal::NumV(10.0);
        assert_eq!(input.num_or_else(throw!(Generic => "test")).unwrap(), 10.0);
    }

    #[test]
    fn num_or_else_test_bad() {
        let input = SteelVal::CharV('f');
        assert!(input.num_or_else(throw!(Generic => "test")).is_err());
    }

    #[test]
    fn char_or_else_test_good() {
        let input = SteelVal::CharV('f');
        assert_eq!(input.char_or_else(throw!(Generic => "test")).unwrap(), 'f');
    }

    #[test]
    fn char_or_else_test_bad() {
        let input = SteelVal::NumV(10.0);
        assert!(input.char_or_else(throw!(Generic => "test")).is_err());
    }

    #[test]
    fn vector_or_else_test_good() {
        let input = SteelVal::VectorV(Gc::new(vector![SteelVal::IntV(1)]));
        assert_eq!(
            input.vector_or_else(throw!(Generic => "test")).unwrap(),
            vector![SteelVal::IntV(1)]
        );
    }

    #[test]
    fn vector_or_else_bad() {
        let input = SteelVal::CharV('f');
        assert!(input.vector_or_else(throw!(Generic => "test")).is_err());
    }

    #[test]
    fn void_or_else_test_good() {
        let input = SteelVal::Void;
        assert_eq!(input.void_or_else(throw!(Generic => "test")).unwrap(), ())
    }

    #[test]
    fn void_or_else_test_bad() {
        let input = SteelVal::StringV("foo".into());
        assert!(input.void_or_else(throw!(Generic => "test")).is_err());
    }

    #[test]
    fn string_or_else_test_good() {
        let input = SteelVal::StringV("foo".into());
        assert_eq!(
            input.string_or_else(throw!(Generic => "test")).unwrap(),
            "foo".to_string()
        );
    }

    #[test]
    fn string_or_else_test_bad() {
        let input = SteelVal::Void;
        assert!(input.string_or_else(throw!(Generic => "test")).is_err())
    }

    #[test]
    fn symbol_or_else_test_good() {
        let input = SteelVal::SymbolV("foo".into());
        assert_eq!(
            input.symbol_or_else(throw!(Generic => "test")).unwrap(),
            "foo".to_string()
        );
    }

    #[test]
    fn symbol_or_else_test_bad() {
        let input = SteelVal::Void;
        assert!(input.symbol_or_else(throw!(Generic => "test")).is_err())
    }
}
