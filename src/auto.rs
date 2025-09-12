use std::task::Poll;

use abi_stable::std_types::{ROption, RResult, Tuple2};
use async_ffi::FfiPoll;

use crate::Started;

trait FromOption {
    type T;
    fn from_option(option: Option<Self::T>) -> Self;
}

impl<T> FromOption for ROption<T> {
    type T = T;

    fn from_option(option: Option<Self::T>) -> Self {
        option.into()
    }
}

trait FromResult {
    type T;
    type E;
    fn from_result(result: Result<Self::T, Self::E>) -> Self;
}

impl<T, E> FromResult for RResult<T, E> {
    type T = T;
    type E = E;

    fn from_result(result: Result<Self::T, Self::E>) -> Self {
        result.into()
    }
}

pub(crate) trait Auto<T, X = ()> {
    fn auto(self) -> T;
}

impl<T> Auto<T> for T {
    fn auto(self) -> T {
        self
    }
}

impl<T, X, U: Auto<T, X>> Auto<Option<T>, (X,)> for Option<U> {
    fn auto(self) -> Option<T> {
        self.map(Auto::auto)
    }
}

impl<T, U: Auto<T>> Auto<ROption<T>> for Option<U> {
    fn auto(self) -> ROption<T> {
        ROption::from_option(self.auto())
    }
}

impl<T, U: Auto<T>> Auto<Option<T>> for ROption<U> {
    fn auto(self) -> Option<T> {
        self.into_option().auto()
    }
}

impl<T, X, U: Auto<T, X>> Auto<Poll<T>, (X,)> for Poll<U> {
    fn auto(self) -> Poll<T> {
        self.map(Auto::auto)
    }
}

impl<T, U: Auto<T>> Auto<FfiPoll<T>> for Poll<U> {
    fn auto(self) -> FfiPoll<T> {
        FfiPoll::from_poll(self.auto())
    }
}

impl<T, X, U: Auto<T, X>> Auto<Poll<T>, X> for FfiPoll<U> {
    fn auto(self) -> Poll<T> {
        self.try_into_poll().expect("poll panicked").auto()
    }
}

impl<T0, T1, U0: Auto<T0>, U1: Auto<T1>> Auto<Tuple2<T0, T1>> for (U0, U1) {
    fn auto(self) -> Tuple2<T0, T1> {
        Tuple2(self.0.auto(), self.1.auto())
    }
}

impl<T0, T1, U0: Auto<T0>, U1: Auto<T1>> Auto<(T0, T1)> for Tuple2<U0, U1> {
    fn auto(self) -> (T0, T1) {
        (self.0.auto(), self.1.auto())
    }
}

impl<T0, T1, U0: Auto<T0>, U1: Auto<T1>> Auto<Result<T0, T1>, ((),)> for Result<U0, U1> {
    fn auto(self) -> Result<T0, T1> {
        self.map(Auto::auto).map_err(Auto::auto)
    }
}

impl<T0, T1, U0: Auto<T0>, U1: Auto<T1>> Auto<RResult<T0, T1>> for Result<U0, U1> {
    fn auto(self) -> RResult<T0, T1> {
        RResult::from_result(self.auto())
    }
}

impl<T0, T1, U0: Auto<T0>, U1: Auto<T1>> Auto<Result<T0, T1>> for RResult<U0, U1> {
    fn auto(self) -> Result<T0, T1> {
        self.into_result().auto()
    }
}

impl<T, U: Auto<T>> Auto<Result<(), T>> for Started<U> {
    fn auto(self) -> Result<(), T> {
        self.try_into_result()
            .unwrap_or_else(|_| panic!("FFI start_send panicked"))
            .auto()
    }
}
