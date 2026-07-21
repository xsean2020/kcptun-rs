//! A string allocation library. This is primarily useful when you want to allocate a bunch of
//! small strings, use them, and then destroy them all together.
//!
//! ## Example
//!
//! ```
//! use str_stack::StrStack;
//!
//! let mut stack = StrStack::new();
//! let first = stack.push("one");
//! let second = stack.push("two");
//! let third = stack.push("three");
//!
//! assert_eq!(&stack[first], "one");
//! assert_eq!(&stack[second], "two");
//! assert_eq!(&stack[third], "three");
//! ```
//!
use std::fmt::{self, Write};
use std::io;
use std::iter::FromIterator;
use std::ops::Index;
use std::slice;

#[derive(Clone)]
pub struct StrStack {
    data: String,
    ends: Vec<usize>,
}

impl Default for StrStack {
    fn default() -> Self {
        Self::new()
    }
}

impl Index<usize> for StrStack {
    type Output = str;
    #[inline]
    fn index(&self, index: usize) -> &str {
        unsafe {
            assert!(index < self.len(), "index out of bounds");
            self.get_unchecked(index)
        }
    }
}

#[derive(Clone)]
pub struct Iter<'a> {
    data: &'a str,
    ends: &'a [usize],
}

impl fmt::Debug for StrStack {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a str;
    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        unsafe {
            let len = self.ends.len();
            if len == 1 {
                None
            } else {
                let start = *self.ends.get_unchecked(0);
                let end = *self.ends.get_unchecked(1);
                self.ends = slice::from_raw_parts(self.ends.as_ptr().offset(1), len - 1);
                Some(self.data.get_unchecked(start..end))
            }
        }
    }

    fn count(self) -> usize {
        self.size_hint().0
    }

    fn last(mut self) -> Option<&'a str> {
        self.next_back()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.ends.len() - 1;
        (len, Some(len))
    }
}

impl<'a> ExactSizeIterator for Iter<'a> {}

impl<'a> DoubleEndedIterator for Iter<'a> {
    #[inline]
    fn next_back(&mut self) -> Option<&'a str> {
        unsafe {
            let len = self.ends.len();
            if len == 1 {
                None
            } else {
                let start = *self.ends.get_unchecked(len - 2);
                let end = *self.ends.get_unchecked(len - 1);
                self.ends = slice::from_raw_parts(self.ends.as_ptr(), len - 1);
                Some(self.data.get_unchecked(start..end))
            }
        }
    }
}

impl<'a> IntoIterator for &'a StrStack {
    type IntoIter = Iter<'a>;
    type Item = &'a str;
    #[inline]
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl StrStack {
    /// Create a new StrStack.
    #[inline]
    pub fn new() -> StrStack {
        StrStack::with_capacity(0, 0)
    }

    /// Create a new StrStack with the given capacity.
    ///
    /// You will be able to push `bytes` bytes and create `strings` strings before reallocating.
    #[inline]
    pub fn with_capacity(bytes: usize, strings: usize) -> StrStack {
        let mut stack = StrStack {
            data: String::with_capacity(bytes),
            ends: Vec::with_capacity(strings + 1),
        };
        // Yes, I know I don't need this. However, putting this here avoids checks later which
        // makes this much faster.
        stack.ends.push(0);
        stack
    }

    /// Push a string onto the string stack.
    ///
    /// This returns the index of the string on the stack.
    #[inline]
    pub fn push(&mut self, s: &str) -> usize {
        self.data.push_str(s);
        self.ends.push(self.data.len());
        self.len() - 1
    }

    /// Iterate over the strings on the stack.
    #[inline]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            data: &self.data,
            ends: &self.ends,
        }
    }

    /// Remove the top string from the stack.
    ///
    /// Returns true iff a string was removed.
    #[inline]
    pub fn pop(&mut self) -> bool {
        if self.ends.len() <= 1 {
            false
        } else {
            self.ends.pop();
            self.data.truncate(*self.ends.last().unwrap());
            true
        }
    }

    /// Clear the stack.
    #[inline]
    pub fn clear(&mut self) {
        self.ends.truncate(1);
        self.data.clear();
    }

    /// Returns the number of strings on the stack.
    #[inline]
    pub fn len(&self) -> usize {
        self.ends.len() - 1
    }

    /// Returns true if there are no strings on the stack.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ends.len() == 1
    }

    /// Truncate the stack to `len` strings.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.ends.truncate(len.saturating_add(1));
        self.data.truncate(*self.ends.last().unwrap());
    }

    /// Read from `source` into the string stack.
    ///
    /// Returns the index of the new string or an IO Error.
    pub fn consume<R: io::Read>(&mut self, mut source: R) -> io::Result<usize> {
        match source.read_to_string(&mut self.data) {
            Ok(_) => {
                self.ends.push(self.data.len());
                Ok(self.len() - 1)
            }
            Err(e) => Err(e),
        }
    }

    /// Returns a writer helper for this string stack.
    ///
    /// This is useful for building a string in-place on the string-stack.
    ///
    /// Example:
    ///
    /// ```
    /// use std::fmt::Write;
    /// use str_stack::StrStack;
    ///
    /// let mut s = StrStack::new();
    /// let index = {
    ///     let mut writer = s.writer();
    ///     writer.write_str("Hello");
    ///     writer.write_char(' ');
    ///     writer.write_str("World");
    ///     writer.write_char('!');
    ///     writer.finish()
    /// };
    /// assert_eq!(&s[index], "Hello World!");
    /// ```
    #[inline]
    pub fn writer(&mut self) -> Writer<'_> {
        Writer(self)
    }

    /// Allows calling the write! macro directly on the string stack:
    ///
    /// Example:
    ///
    /// ```
    /// use std::fmt::Write;
    /// use str_stack::StrStack;
    ///
    /// let mut s = StrStack::new();
    /// let index = write!(&mut s, "Hello {}!", "World");
    /// assert_eq!(&s[index], "Hello World!");
    /// ```
    #[inline]
    pub fn write_fmt(&mut self, args: fmt::Arguments) -> usize {
        let mut writer = self.writer();
        let _ = writer.write_fmt(args);
        writer.finish()
    }

    /// Returns a reference to the string at the given index, without performing bounds checking.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `index < self.len()`.
    ///
    /// # Example
    ///
    /// ```
    /// use str_stack::StrStack;
    ///
    /// let mut stack = StrStack::new();
    /// stack.push("one");
    /// stack.push("two");
    ///
    /// unsafe {
    ///     assert_eq!(stack.get_unchecked(0), "one");
    ///     assert_eq!(stack.get_unchecked(1), "two");
    /// }
    /// ```
    #[inline]
    pub unsafe fn get_unchecked(&self, index: usize) -> &str {
        let start = *self.ends.get_unchecked(index);
        let end = *self.ends.get_unchecked(index + 1);
        self.data.get_unchecked(start..end)
    }
}

impl<S> Extend<S> for StrStack
where
    S: AsRef<str>,
{
    fn extend<T>(&mut self, iterator: T)
    where
        T: IntoIterator<Item = S>,
    {
        let iterator = iterator.into_iter();
        let (min, _) = iterator.size_hint();
        self.ends.reserve(min);
        for v in iterator {
            self.push(v.as_ref());
        }
    }
}

impl<S> FromIterator<S> for StrStack
where
    S: AsRef<str>,
{
    fn from_iter<T>(iterator: T) -> Self
    where
        T: IntoIterator<Item = S>,
    {
        let mut stack = StrStack::new();
        stack.extend(iterator);
        stack
    }
}

pub struct Writer<'a>(&'a mut StrStack);

impl<'a> Writer<'a> {
    /// Finish pushing the string onto the stack and return its index.
    #[inline]
    pub fn finish(self) -> usize {
        // We push on drop.
        self.0.len()
    }
}

impl<'a> fmt::Write for Writer<'a> {
    #[inline]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.data.push_str(s);
        Ok(())
    }
    #[inline]
    fn write_char(&mut self, c: char) -> fmt::Result {
        self.0.data.push(c);
        Ok(())
    }
}

impl<'a> Drop for Writer<'a> {
    fn drop(&mut self) {
        self.0.ends.push(self.0.data.len());
    }
}

#[test]
fn test_basic() {
    for mut stack in [StrStack::new(), StrStack::default()] {
        let first = stack.push("one");
        let second = stack.push("two");
        let third = stack.push("three");

        assert_eq!(&stack[first], "one");
        assert_eq!(&stack[second], "two");
        assert_eq!(&stack[third], "three");

        assert_eq!(stack.len(), 3);

        assert!(stack.pop());

        assert_eq!(stack.len(), 2);

        assert_eq!(&stack[first], "one");
        assert_eq!(&stack[second], "two");

        assert!(stack.pop());
        assert!(stack.pop());

        assert_eq!(stack.len(), 0);
        assert!(!stack.pop());
    }
}

#[test]
fn test_consume() {
    let mut stack = StrStack::new();
    let idx = stack.consume("testing".as_bytes()).unwrap();
    assert_eq!(&stack[idx], "testing");
}

#[test]
fn test_writer() {
    let mut stack = StrStack::new();
    let first = {
        let mut w = stack.writer();
        write!(w, "{}", "first ").unwrap();
        write!(w, "{}", "second").unwrap();
        w.finish()
    };

    let second = {
        let mut w = stack.writer();
        write!(w, "{}", "third ").unwrap();
        write!(w, "{}", "fourth").unwrap();
        w.finish()
    };
    assert_eq!(&stack[first], "first second");
    assert_eq!(&stack[second], "third fourth");
}

#[test]
fn test_iter() {
    let mut stack = StrStack::new();
    stack.push("one");
    stack.push("two");
    stack.push("three");

    let v1: Vec<_> = stack.iter().collect();
    let v2: Vec<_> = stack.iter().rev().collect();

    assert_eq!(&v1[..], &["one", "two", "three"]);
    assert_eq!(&v2[..], &["three", "two", "one"]);
}
