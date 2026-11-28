//! Types that have the same fields on the wire as here.

/// Both conversions between two structs with the same fields, so that a
/// field cannot be carried one way and dropped the other. A field marked
/// `: option` or `: each` holds values that are converted in turn.
macro_rules! mirror {
    ($ours:ty, $theirs:ty { $($field:ident $(: $kind:ident)?),* $(,)? }) => {
        $crate::mirror::mirror!(@one $ours => $theirs { $($field $(: $kind)?),* });
        $crate::mirror::mirror!(@one $theirs => $ours { $($field $(: $kind)?),* });
    };
    (@one $from:ty => $to:ty { $($field:ident $(: $kind:ident)?),* }) => {
        impl From<$from> for $to {
            fn from(value: $from) -> Self {
                Self {
                    $($field: $crate::mirror::mirror!(@take value.$field $(: $kind)?)),*
                }
            }
        }
    };
    (@take $value:ident.$field:ident) => {
        $value.$field
    };
    (@take $value:ident.$field:ident: option) => {
        $value.$field.map(Into::into)
    };
    (@take $value:ident.$field:ident: each) => {
        $value.$field.into_iter().map(Into::into).collect()
    };
}

pub(crate) use mirror;
