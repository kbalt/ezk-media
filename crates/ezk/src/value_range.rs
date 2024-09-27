use std::{
    fmt,
    ops::{Add, RangeInclusive, Sub},
};

/// Describes a range of allowed/supported values
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValueRange<T> {
    /// Any of the given values, sorted by highest to lowest priority
    AnyOf(Vec<Self>),
    /// Inclusive range of values
    Range(Range<T>),
    /// Single value
    Value(T),
}

impl<T> From<T> for ValueRange<T> {
    fn from(value: T) -> Self {
        ValueRange::Value(value)
    }
}

impl<T> From<Range<T>> for ValueRange<T> {
    fn from(value: Range<T>) -> Self {
        ValueRange::Range(value)
    }
}

impl<T> From<Vec<T>> for ValueRange<T> {
    fn from(value: Vec<T>) -> Self {
        ValueRange::AnyOf(value.into_iter().map(ValueRange::Value).collect())
    }
}

impl<T> From<Vec<ValueRange<T>>> for ValueRange<T> {
    fn from(value: Vec<ValueRange<T>>) -> Self {
        ValueRange::AnyOf(value)
    }
}

impl<T> ValueRange<T>
where
    T: Clone + PartialOrd + Ord + fmt::Debug,
{
    pub fn range(lower_bound: T, upper_bound: T) -> Self {
        assert!(lower_bound <= upper_bound);

        if lower_bound == upper_bound {
            Self::Value(lower_bound)
        } else {
            Self::Range(Range(lower_bound, upper_bound))
        }
    }

    /// Returns the first value in the range
    pub fn first_value(&self) -> T {
        match self {
            Self::AnyOf(v) => v[0].first_value(),
            Self::Range(Range(l, _)) => l.clone(),
            Self::Value(v) => v.clone(),
        }
    }

    /// Returns if the given value is contained in this range
    pub fn contains(&self, item: &T) -> bool {
        match self {
            Self::AnyOf(values) => values.iter().any(|v| v.contains(item)),
            Self::Range(Range(l, u)) => (l..=u).contains(&item),
            Self::Value(v) => v == item,
        }
    }

    /// Create an intersection from two [`ValueRange`]s
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        match (self, other) {
            (Self::AnyOf(ao1), Self::AnyOf(ao2)) => {
                let mut values = vec![];

                for v1 in ao1 {
                    for v2 in ao2 {
                        if let Some(v) = v1.intersect(v2) {
                            if !values.contains(&v) {
                                values.push(v);
                            }
                        }
                    }
                }

                Self::from_values(values)
            }
            (Self::Range(Range(l1, u1)), Self::Range(Range(l2, u2))) => {
                if u1 < l2 || u2 < l1 {
                    None
                } else {
                    let l = l1.max(l2);
                    let u = u1.min(u2);

                    if l == u {
                        Some(Self::Value(l.clone()))
                    } else {
                        Some(Self::Range(Range(l.clone(), u.clone())))
                    }
                }
            }
            (Self::AnyOf(values), value @ (Self::Value(_) | Self::Range(_)))
            | (value @ (Self::Value(_) | Self::Range(_)), Self::AnyOf(values)) => {
                let mut r = vec![];

                for v in values {
                    if let Some(v) = v.intersect(value) {
                        if !r.contains(&v) {
                            r.push(v);
                        }
                    }
                }

                Self::from_values(r)
            }
            (Self::Range(Range(l, u)), Self::Value(v))
            | (Self::Value(v), Self::Range(Range(l, u))) => {
                (l..=u).contains(&v).then(|| Self::Value(v.clone()))
            }
            (Self::Value(v1), Self::Value(v2)) => (v1 == v2).then(|| Self::Value(v1.clone())),
        }
    }

    fn from_values(mut values: Vec<Self>) -> Option<Self> {
        match values.len() {
            0 => None,
            1 => Some(values.remove(0)),
            _ => Some(Self::AnyOf(values)),
        }
    }

    pub fn find_map<F, U>(&self, mut f: F) -> Option<U>
    where
        F: FnMut(T) -> Option<U>,
        RangeInclusive<T>: Iterator<Item = T>,
    {
        match self {
            ValueRange::AnyOf(values) => {
                for value in values {
                    if let Some(v) = value.find_map(&mut f) {
                        return Some(v);
                    }
                }
            }
            ValueRange::Range(Range(l, u)) => {
                for v in l.clone()..=u.clone() {
                    if let Some(u) = f(v) {
                        return Some(u);
                    }
                }
            }
            ValueRange::Value(v) => {
                return f(v.clone());
            }
        }

        None
    }
}

impl<T> ValueRange<T>
where
    T: Clone + PartialOrd + Ord + fmt::Debug,
    T: From<u8> + Add<Output = T> + Sub<Output = T>,
{
    pub fn without(&self, t: &T) -> Option<Self> {
        match self {
            ValueRange::AnyOf(any_of) => {
                Self::from_values(any_of.iter().filter_map(|v| v.without(t)).collect())
            }
            ValueRange::Range(Range(l, u)) => {
                if (l..=u).contains(&t) {
                    let mut values = vec![];

                    if l != t {
                        values.push(ValueRange::range(l.clone(), t.clone() - T::from(1)));
                    }

                    if u != t {
                        values.push(ValueRange::range(t.clone() + T::from(1), u.clone()));
                    }

                    Self::from_values(values)
                } else {
                    Some(self.clone())
                }
            }
            ValueRange::Value(v) => {
                if v == t {
                    None
                } else {
                    Some(self.clone())
                }
            }
        }
    }
}

impl<T> fmt::Debug for ValueRange<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnyOf(values) => {
                let mut values = values.iter().peekable();

                while let Some(v) = values.next() {
                    if matches!(v, Self::Value(_)) {
                        write!(f, "{v:?}")?;
                    } else {
                        write!(f, "({v:?})")?;
                    }

                    if values.peek().is_some() {
                        write!(f, " | ")?;
                    }
                }

                Ok(())
            }
            Self::Range(Range(v1, v2)) => write!(f, "{v1:?}..={v2:?}"),
            Self::Value(v) => v.fmt(f),
        }
    }
}

#[macro_export]
macro_rules! any_of {
    ($($expr:expr),+ $(,)?) => {
        $crate::ValueRange::AnyOf(vec![
            $($crate::ValueRange::from($expr),)*
        ])
    };
}

pub trait Intersect<T>: Sized {
    type Output;

    fn intersect(&self, other: &Self) -> Option<Self::Output>;
}

impl<T> Intersect<Vec<T>> for Vec<T>
where
    T: Intersect<T, Output: PartialEq>,
{
    type Output = Vec<T::Output>;

    fn intersect(&self, other: &Self) -> Option<Self::Output> {
        let mut values = vec![];

        for v1 in self {
            for v2 in other {
                if let Some(v) = v1.intersect(v2) {
                    if !values.contains(&v) {
                        values.push(v);
                    }
                }
            }
        }

        if values.is_empty() {
            None
        } else {
            Some(values)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Range<T>(pub T, pub T);

impl<T> Intersect<Range<T>> for Range<T>
where
    T: Clone + Ord,
{
    type Output = Range<T>;

    fn intersect(&self, other: &Self) -> Option<Self::Output> {
        let Range(l1, u1) = self;
        let Range(l2, u2) = other;

        if u1 < l2 || u2 < l1 {
            None
        } else {
            let l = l1.max(l2);
            let u = u1.min(u2);

            Some(Range(l.clone(), u.clone()))
        }
    }
}

impl<T> Range<T>
where
    T: PartialOrd,
{
    pub fn contains(&self, item: &T) -> bool {
        (&self.0..=&self.1).contains(&item)
    }
}

#[cfg(test)]
mod tests {
    use super::{Range, ValueRange};
    // use super::ValueRange::*;
    use std::fmt::Debug;

    #[track_caller]
    fn assert_intersect<T>(
        v1: impl Into<ValueRange<T>>,
        v2: impl Into<ValueRange<T>>,
        expected: Option<ValueRange<T>>,
    ) where
        T: Clone + Copy + PartialEq + Eq + PartialOrd + Ord + Debug,
    {
        let v1 = v1.into();
        let v2 = v2.into();

        assert_eq!(v1.intersect(&v2), expected);
        assert_eq!(v2.intersect(&v1), expected);
    }

    #[test]
    fn intersect() {
        let v1 = 1;
        let v2 = 2;
        assert_intersect::<i32>(v1, v2, None);

        let v1 = 1;
        let v2 = Range(0, 123);
        assert_intersect::<i32>(v1, v2, Some(1.into()));

        let v1 = Range(1, 20);
        let v2 = Range(0, 123);
        assert_intersect::<i32>(v1, v2, Some(ValueRange::range(1, 20)));

        let v1 = Range(1, 200);
        let v2 = Range(0, 123);
        assert_intersect::<i32>(v1, v2, Some(ValueRange::range(1, 123)));

        let v1 = Range(1, 10);
        let v2 = Range(100, 1000);
        assert_intersect::<i32>(v1, v2, None);

        let v1 = 5;
        let v2 = any_of![1, Range(3, 6)];
        assert_intersect::<i32>(v1, v2, Some(5.into()));

        let v1 = Range(5, 1000);
        let v2 = any_of![1, Range(3, 6)];
        assert_intersect::<i32>(v1, v2, Some(ValueRange::Range(Range(5, 6))));

        let v1 = any_of![1, 5, Range(10, 1000)];
        let v2: ValueRange<i32> = any_of![4, 500];
        assert_intersect::<i32>(v1, v2, Some(500.into()));

        let v1 = any_of![Range(10, 20), Range(30, 40)];
        let v2 = any_of![Range(1, 20), Range(20, 100)];
        assert_intersect::<i32>(v1, v2, Some(any_of![Range(10, 20), 20, Range(30, 40)]));

        let v1 = any_of![8000, Range(7000, 9000)];
        let v2 = 8000;
        assert_intersect::<i32>(v1, v2, Some(8000.into()));
    }

    #[test]
    fn without() {
        // use ValueRange::*;

        // let v = ValueRange::Value(1);
        // assert_eq!(v.without(&1), None);

        // let v = Range(1, 10);
        // assert_eq!(v.without(&1), Some(Range(2, 10)));
        // assert_eq!(v.without(&2), Some(AnyOf(vec![Value(1), Range(3, 10)])));
        // assert_eq!(v.without(&5), Some(AnyOf(vec![Range(1, 4), Range(6, 10)])));
        // assert_eq!(v.without(&9), Some(AnyOf(vec![Range(1, 8), Value(10)])));
        // assert_eq!(v.without(&10), Some(Range(1, 9)));

        // let v = AnyOf(vec![Value(1), Value(3)]);
        // assert_eq!(v.without(&3), Some(Value(1)));
    }
}
