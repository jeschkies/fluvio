mod spec;
mod status;

pub use self::spec::*;
pub use self::status::*;

#[cfg(feature = "k8")]
mod k8;
#[cfg(feature = "k8")]
pub use k8::*;

mod metadata {

    use crate::core::{Spec, Status, Removable, Creatable};
    use crate::extended::{SpecExt, ObjectType};

    use super::*;

    impl Spec for SmartStreamSpec {
        const LABEL: &'static str = "SmartStream";
        type IndexKey = String;
        type Status = SmartStreamStatus;
        type Owner = Self;
    }

    impl SpecExt for SmartStreamSpec {
        const OBJECT_TYPE: ObjectType = ObjectType::Topic;
    }

    impl Removable for SmartStreamSpec {
        type DeleteKey = String;
    }

    impl Creatable for SmartStreamSpec {}

    impl Status for SmartStreamStatus {}

    #[cfg(feature = "k8")]
    mod extended {

        use crate::store::k8::K8ExtendedSpec;
        use crate::store::k8::K8ConvertError;
        use crate::store::k8::K8MetaItem;
        use crate::store::MetadataStoreObject;
        use crate::k8_types::K8Obj;
        use crate::store::k8::default_convert_from_k8;

        use super::SmartStreamSpec;

        impl K8ExtendedSpec for SmartStreamSpec {
            type K8Spec = Self;
            type K8Status = Self::Status;

            fn convert_from_k8(
                k8_obj: K8Obj<Self::K8Spec>,
            ) -> Result<MetadataStoreObject<Self, K8MetaItem>, K8ConvertError<Self::K8Spec>>
            {
                default_convert_from_k8(k8_obj)
            }
        }
    }
}
