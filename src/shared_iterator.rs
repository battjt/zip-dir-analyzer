use std::{cell::RefCell, rc::Rc};

pub struct SharedIterator<'a, T> {
    iter: Rc<RefCell<&'a mut dyn Iterator<Item = T>>>,
}
impl<'a, T> Clone for SharedIterator<'a, T> {
    fn clone(&self) -> Self {
        Self {
            iter: self.iter.clone(),
        }
    }
}
impl<'a, T> SharedIterator<'a, T> {
    pub fn new(iter: &'a mut dyn Iterator<Item = T>) -> Self {
        Self {
            iter: Rc::new(RefCell::new(iter)),
        }
    }
}

impl<'a, T> Iterator for SharedIterator<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let mut iterator = self.iter.borrow_mut();
        iterator.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let mut i = [1, 2, 3, 4].iter();
        let mut shared1 = SharedIterator::new(&mut i);
        let mut shared2 = shared1.clone();
        assert_eq!(1, *shared1.next().unwrap());
        assert_eq!(2, *shared2.next().unwrap());
        assert_eq!(3, *shared2.next().unwrap());
        assert_eq!(4, *shared1.next().unwrap());
        assert_eq!(None, shared1.next());
        assert_eq!(None, shared2.next());
    }
}
