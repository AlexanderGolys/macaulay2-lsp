//! Shared object identity and lookup over semantic object stores.

use crate::builtin_index::{BuiltinData, InstanceID, ObjectId, Record};

/// Identity and record lookup shared by every semantic object source.
pub trait ObjectKnowledge {
    /// Borrow the object with `object_id`.
    fn object(&self, object_id: ObjectId) -> Option<&Record>;

    /// Resolve a canonical name or alias to its object identity.
    fn resolve_object(&self, name: &InstanceID) -> Option<ObjectId>;

    /// Resolve a canonical name or alias to its object record.
    fn get_record(&self, name: &InstanceID) -> Option<&Record> {
        self.object(self.resolve_object(name)?)
    }
}

impl BuiltinData {
    /// Borrow the record named by `name`, resolving aliases through the canonical
    /// index.
    pub fn get_record(&self, name: &InstanceID) -> Option<&Record> {
        self.index.record(name)
    }

    /// Borrow one canonical object by its opaque identity.
    pub fn object(&self, object_id: ObjectId) -> Option<&Record> {
        self.index.object(object_id)
    }

    /// Resolve a canonical name or alias to its object identity.
    pub fn object_id(&self, name: &InstanceID) -> Option<ObjectId> {
        self.index.object_id(name)
    }
}

impl ObjectKnowledge for BuiltinData {
    fn object(&self, object_id: ObjectId) -> Option<&Record> {
        BuiltinData::object(self, object_id)
    }

    fn resolve_object(&self, name: &InstanceID) -> Option<ObjectId> {
        BuiltinData::object_id(self, name)
    }
}

impl<T: ObjectKnowledge + ?Sized> ObjectKnowledge for &T {
    fn object(&self, object_id: ObjectId) -> Option<&Record> {
        T::object(self, object_id)
    }

    fn resolve_object(&self, name: &InstanceID) -> Option<ObjectId> {
        T::resolve_object(self, name)
    }
}

#[cfg(test)]
impl ObjectKnowledge for crate::typesystem::NoTypeKnowledge {
    fn object(&self, _object_id: ObjectId) -> Option<&Record> {
        None
    }

    fn resolve_object(&self, _name: &InstanceID) -> Option<ObjectId> {
        None
    }
}
