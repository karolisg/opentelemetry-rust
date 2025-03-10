//! Primitives for sending name/value data across system boundaries.
//!
//! Baggage is used to annotate telemetry, adding context and information to
//! metrics, traces, and logs. It is a set of name/value pairs describing
//! user-defined properties. Each name in Baggage is associated with exactly one
//! value.
//!
//! Main types in this module are:
//!
//! * [`Baggage`]: A set of name/value pairs describing user-defined properties.
//! * [`BaggageExt`]: Extensions for managing `Baggage` in a [`Context`].
//!
//! Baggage can be sent between systems using a baggage propagator in
//! accordance with the [W3C Baggage] specification.
//!
//! [W3C Baggage]: https://w3c.github.io/baggage
use crate::{Context, Key, KeyValue, Value};
use once_cell::sync::Lazy;
use std::collections::{hash_map, HashMap};
use std::fmt;
use std::iter::FromIterator;
use urlencoding::encode;

static DEFAULT_BAGGAGE: Lazy<Baggage> = Lazy::new(Baggage::default);

const MAX_KEY_VALUE_PAIRS: usize = 180;
const MAX_BYTES_FOR_ONE_PAIR: usize = 4096;
const MAX_LEN_OF_ALL_PAIRS: usize = 8192;

/// A set of name/value pairs describing user-defined properties.
///
/// ### Baggage Names
///
/// * ASCII strings according to the token format, defined in [RFC2616, Section 2.2]
///
/// ### Baggage Values
///
/// * URL encoded UTF-8 strings.
///
/// ### Baggage Value Metadata
///
/// Additional metadata can be added to values in the form of a property set,
/// represented as semi-colon `;` delimited list of names and/or name/value pairs,
/// e.g. `;k1=v1;k2;k3=v3`.
///
/// ### Limits
///
/// * Maximum number of name/value pairs: `180`.
/// * Maximum number of bytes per a single name/value pair: `4096`.
/// * Maximum total length of all name/value pairs: `8192`.
///
/// [RFC2616, Section 2.2]: https://tools.ietf.org/html/rfc2616#section-2.2
#[derive(Debug, Default)]
pub struct Baggage {
    inner: HashMap<Key, (Value, BaggageMetadata)>,
    kv_content_len: usize, // the length of key-value-metadata string in `inner`
}

impl Baggage {
    /// Creates an empty `Baggage`.
    pub fn new() -> Self {
        Baggage {
            inner: HashMap::default(),
            kv_content_len: 0,
        }
    }

    /// Returns a reference to the value associated with a given name
    ///
    /// # Examples
    ///
    /// ```
    /// use opentelemetry_api::{baggage::Baggage, Value};
    ///
    /// let mut cc = Baggage::new();
    /// let _ = cc.insert("my-name", "my-value");
    ///
    /// assert_eq!(cc.get("my-name"), Some(&Value::from("my-value")))
    /// ```
    pub fn get<T: Into<Key>>(&self, key: T) -> Option<&Value> {
        self.inner.get(&key.into()).map(|(value, _metadata)| value)
    }

    /// Returns a reference to the value and metadata associated with a given name
    ///
    /// # Examples
    /// ```
    /// use opentelemetry_api::{baggage::{Baggage, BaggageMetadata}, Value};
    ///
    /// let mut cc = Baggage::new();
    /// let _ = cc.insert("my-name", "my-value");
    ///
    /// // By default, the metadata is empty
    /// assert_eq!(cc.get_with_metadata("my-name"), Some(&(Value::from("my-value"), BaggageMetadata::from(""))))
    /// ```
    pub fn get_with_metadata<T: Into<Key>>(&self, key: T) -> Option<&(Value, BaggageMetadata)> {
        self.inner.get(&key.into())
    }

    /// Inserts a name/value pair into the baggage.
    ///
    /// If the name was not present, [`None`] is returned. If the name was present,
    /// the value is updated, and the old value is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use opentelemetry_api::{baggage::Baggage, Value};
    ///
    /// let mut cc = Baggage::new();
    /// let _ = cc.insert("my-name", "my-value");
    ///
    /// assert_eq!(cc.get("my-name"), Some(&Value::from("my-value")))
    /// ```
    pub fn insert<K, V>(&mut self, key: K, value: V) -> Option<Value>
    where
        K: Into<Key>,
        V: Into<Value>,
    {
        self.insert_with_metadata(key, value, BaggageMetadata::default())
            .map(|pair| pair.0)
    }

    /// Inserts a name/value pair into the baggage.
    ///
    /// Same with `insert`, if the name was not present, [`None`] will be returned.
    /// If the name is present, the old value and metadata will be returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use opentelemetry_api::{baggage::{Baggage, BaggageMetadata}, Value};
    ///
    /// let mut cc = Baggage::new();
    /// let _ = cc.insert_with_metadata("my-name", "my-value", "test");
    ///
    /// assert_eq!(cc.get_with_metadata("my-name"), Some(&(Value::from("my-value"), BaggageMetadata::from("test"))))
    /// ```
    pub fn insert_with_metadata<K, V, S>(
        &mut self,
        key: K,
        value: V,
        metadata: S,
    ) -> Option<(Value, BaggageMetadata)>
    where
        K: Into<Key>,
        V: Into<Value>,
        S: Into<BaggageMetadata>,
    {
        let (key, value, metadata) = (key.into(), value.into(), metadata.into());
        if self.insertable(&key, &value, &metadata) {
            self.inner.insert(key, (value, metadata))
        } else {
            None
        }
    }

    /// Removes a name from the baggage, returning the value
    /// corresponding to the name if the pair was previously in the map.
    pub fn remove<K: Into<Key>>(&mut self, key: K) -> Option<(Value, BaggageMetadata)> {
        self.inner.remove(&key.into())
    }

    /// Returns the number of attributes for this baggage
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the baggage contains no items.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Gets an iterator over the baggage items, sorted by name.
    pub fn iter(&self) -> Iter<'_> {
        self.into_iter()
    }

    /// Determine whether the key value pair exceed one of the [limits](https://w3c.github.io/baggage/#limits).
    /// If not, update the total length of key values
    fn insertable(&mut self, key: &Key, value: &Value, metadata: &BaggageMetadata) -> bool {
        if !key.as_str().is_ascii() {
            return false;
        }
        let value = value.as_str();
        if key_value_metadata_bytes_size(key.as_str(), value.as_ref(), metadata.as_str())
            < MAX_BYTES_FOR_ONE_PAIR
        {
            match self.inner.get(key) {
                None => {
                    // check total length
                    if self.kv_content_len
                        + metadata.as_str().len()
                        + value.len()
                        + key.as_str().len()
                        > MAX_LEN_OF_ALL_PAIRS
                    {
                        return false;
                    }
                    // check number of pairs
                    if self.inner.len() + 1 > MAX_KEY_VALUE_PAIRS {
                        return false;
                    }
                    self.kv_content_len +=
                        metadata.as_str().len() + value.len() + key.as_str().len()
                }
                Some((old_value, old_metadata)) => {
                    let old_value = old_value.as_str();
                    if self.kv_content_len - old_metadata.as_str().len() - old_value.len()
                        + metadata.as_str().len()
                        + value.len()
                        > MAX_LEN_OF_ALL_PAIRS
                    {
                        return false;
                    }
                    self.kv_content_len =
                        self.kv_content_len - old_metadata.as_str().len() - old_value.len()
                            + metadata.as_str().len()
                            + value.len()
                }
            }
            true
        } else {
            false
        }
    }
}

/// Get the number of bytes for one key-value pair
fn key_value_metadata_bytes_size(key: &str, value: &str, metadata: &str) -> usize {
    key.bytes().len() + value.bytes().len() + metadata.bytes().len()
}

/// An iterator over the entries of a [`Baggage`].
#[derive(Debug)]
pub struct Iter<'a>(hash_map::Iter<'a, Key, (Value, BaggageMetadata)>);

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a Key, &'a (Value, BaggageMetadata));

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl<'a> IntoIterator for &'a Baggage {
    type Item = (&'a Key, &'a (Value, BaggageMetadata));
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        Iter(self.inner.iter())
    }
}

impl FromIterator<(Key, (Value, BaggageMetadata))> for Baggage {
    fn from_iter<I: IntoIterator<Item = (Key, (Value, BaggageMetadata))>>(iter: I) -> Self {
        let mut baggage = Baggage::default();
        for (key, (value, metadata)) in iter.into_iter() {
            baggage.insert_with_metadata(key, value, metadata);
        }
        baggage
    }
}

impl FromIterator<KeyValue> for Baggage {
    fn from_iter<I: IntoIterator<Item = KeyValue>>(iter: I) -> Self {
        let mut baggage = Baggage::default();
        for kv in iter.into_iter() {
            baggage.insert(kv.key, kv.value);
        }
        baggage
    }
}

impl FromIterator<KeyValueMetadata> for Baggage {
    fn from_iter<I: IntoIterator<Item = KeyValueMetadata>>(iter: I) -> Self {
        let mut baggage = Baggage::default();
        for kvm in iter.into_iter() {
            baggage.insert_with_metadata(kvm.key, kvm.value, kvm.metadata);
        }
        baggage
    }
}

impl fmt::Display for Baggage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, (k, v)) in self.into_iter().enumerate() {
            write!(f, "{}={}", k, encode(&v.0.as_str()))?;
            if !v.1.as_str().is_empty() {
                write!(f, ";{}", v.1)?;
            }

            if i < self.len() - 1 {
                write!(f, ",")?;
            }
        }

        Ok(())
    }
}

/// Methods for sorting and retrieving baggage data in a context.
pub trait BaggageExt {
    /// Returns a clone of the given context with the included name/value pairs.
    ///
    /// # Examples
    ///
    /// ```
    /// use opentelemetry_api::{baggage::BaggageExt, Context, KeyValue, Value};
    ///
    /// let cx = Context::map_current(|cx| {
    ///     cx.with_baggage(vec![KeyValue::new("my-name", "my-value")])
    /// });
    ///
    /// assert_eq!(
    ///     cx.baggage().get("my-name"),
    ///     Some(&Value::from("my-value")),
    /// )
    /// ```
    fn with_baggage<T: IntoIterator<Item = I>, I: Into<KeyValueMetadata>>(
        &self,
        baggage: T,
    ) -> Self;

    /// Returns a clone of the current context with the included name/value pairs.
    ///
    /// # Examples
    ///
    /// ```
    /// use opentelemetry_api::{baggage::BaggageExt, Context, KeyValue, Value};
    ///
    /// let cx = Context::current_with_baggage(vec![KeyValue::new("my-name", "my-value")]);
    ///
    /// assert_eq!(
    ///     cx.baggage().get("my-name"),
    ///     Some(&Value::from("my-value")),
    /// )
    /// ```
    fn current_with_baggage<T: IntoIterator<Item = I>, I: Into<KeyValueMetadata>>(
        baggage: T,
    ) -> Self;

    /// Returns a clone of the given context with no baggage.
    ///
    /// # Examples
    ///
    /// ```
    /// use opentelemetry_api::{baggage::BaggageExt, Context, KeyValue, Value};
    ///
    /// let cx = Context::map_current(|cx| cx.with_cleared_baggage());
    ///
    /// assert_eq!(cx.baggage().len(), 0);
    /// ```
    fn with_cleared_baggage(&self) -> Self;

    /// Returns a reference to this context's baggage, or the default
    /// empty baggage if none has been set.
    fn baggage(&self) -> &Baggage;
}

impl BaggageExt for Context {
    fn with_baggage<T: IntoIterator<Item = I>, I: Into<KeyValueMetadata>>(
        &self,
        baggage: T,
    ) -> Self {
        let mut merged: Baggage = self
            .baggage()
            .iter()
            .map(|(key, (value, metadata))| {
                KeyValueMetadata::new(key.clone(), value.clone(), metadata.clone())
            })
            .collect();
        for kvm in baggage.into_iter().map(|kv| kv.into()) {
            merged.insert_with_metadata(kvm.key, kvm.value, kvm.metadata);
        }

        self.with_value(merged)
    }

    fn current_with_baggage<T: IntoIterator<Item = I>, I: Into<KeyValueMetadata>>(kvs: T) -> Self {
        Context::map_current(|cx| cx.with_baggage(kvs))
    }

    fn with_cleared_baggage(&self) -> Self {
        self.with_value(Baggage::new())
    }

    fn baggage(&self) -> &Baggage {
        self.get::<Baggage>().unwrap_or(&DEFAULT_BAGGAGE)
    }
}

/// An optional property set that can be added to [`Baggage`] values.
///
/// `BaggageMetadata` can be added to values in the form of a property set,
/// represented as semi-colon `;` delimited list of names and/or name/value
/// pairs, e.g. `;k1=v1;k2;k3=v3`.
#[derive(Clone, Debug, PartialOrd, PartialEq, Eq, Default)]
pub struct BaggageMetadata(String);

impl BaggageMetadata {
    /// Return underlying string
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for BaggageMetadata {
    fn from(s: String) -> BaggageMetadata {
        BaggageMetadata(s.trim().to_string())
    }
}

impl From<&str> for BaggageMetadata {
    fn from(s: &str) -> Self {
        BaggageMetadata(s.trim().to_string())
    }
}

impl fmt::Display for BaggageMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(write!(f, "{}", self.as_str())?)
    }
}

/// [`Baggage`] name/value pairs with their associated metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyValueMetadata {
    /// Dimension or event key
    pub key: Key,
    /// Dimension or event value
    pub value: Value,
    /// Metadata associate with this key value pair
    pub metadata: BaggageMetadata,
}

impl KeyValueMetadata {
    /// Create a new `KeyValue` pair with metadata
    pub fn new<K, V, S>(key: K, value: V, metadata: S) -> Self
    where
        K: Into<Key>,
        V: Into<Value>,
        S: Into<BaggageMetadata>,
    {
        KeyValueMetadata {
            key: key.into(),
            value: value.into(),
            metadata: metadata.into(),
        }
    }
}

impl From<KeyValue> for KeyValueMetadata {
    fn from(kv: KeyValue) -> Self {
        KeyValueMetadata {
            key: kv.key,
            value: kv.value,
            metadata: BaggageMetadata::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::StringValue;

    use super::*;

    #[test]
    fn insert_non_ascii_key() {
        let mut baggage = Baggage::new();
        baggage.insert("🚫", "not ascii key");
        assert_eq!(baggage.len(), 0, "did not insert invalid key");
    }

    #[test]
    fn insert_too_much_baggage() {
        // too many key pairs
        let over_limit = MAX_KEY_VALUE_PAIRS + 1;
        let mut data = Vec::with_capacity(over_limit);
        for i in 0..over_limit {
            data.push(KeyValue::new(format!("key{i}"), format!("key{i}")))
        }
        let baggage = data.into_iter().collect::<Baggage>();
        assert_eq!(baggage.len(), MAX_KEY_VALUE_PAIRS)
    }

    #[test]
    fn insert_too_long_pair() {
        let pair = KeyValue::new(
            "test",
            String::from_utf8_lossy(vec![12u8; MAX_BYTES_FOR_ONE_PAIR].as_slice()).to_string(),
        );
        let mut baggage = Baggage::default();
        baggage.insert(pair.key.clone(), pair.value.clone());
        assert_eq!(
            baggage.len(),
            0,
            "The input pair is too long to insert into baggage"
        );

        baggage.insert("test", "value");
        baggage.insert(pair.key.clone(), pair.value);
        assert_eq!(
            baggage.get(pair.key),
            Some(&Value::from("value")),
            "If the input pair is too long, then don't replace entry with same key"
        )
    }

    #[test]
    fn insert_pairs_length_exceed() {
        let mut data = vec![];
        for letter in vec!['a', 'b', 'c', 'd'].into_iter() {
            data.push(KeyValue::new(
                (0..MAX_LEN_OF_ALL_PAIRS / 3)
                    .map(|_| letter)
                    .collect::<String>(),
                "",
            ));
        }
        let baggage = data.into_iter().collect::<Baggage>();
        assert_eq!(baggage.len(), 3)
    }

    #[test]
    fn serialize_baggage_as_string() {
        // Empty baggage
        let b = Baggage::default();
        assert_eq!("", b.to_string());

        // "single member empty value no properties"
        let mut b = Baggage::default();
        b.insert("foo", StringValue::from(""));
        assert_eq!("foo=", b.to_string());

        // "single member no properties"
        let mut b = Baggage::default();
        b.insert("foo", StringValue::from("1"));
        assert_eq!("foo=1", b.to_string());

        // "URL encoded value"
        let mut b = Baggage::default();
        b.insert("foo", StringValue::from("1=1"));
        assert_eq!("foo=1%3D1", b.to_string());

        // "single member empty value with properties"
        let mut b = Baggage::default();
        b.insert_with_metadata(
            "foo",
            StringValue::from(""),
            BaggageMetadata::from("red;state=on"),
        );
        assert_eq!("foo=;red;state=on", b.to_string());

        // "single member with properties"
        let mut b = Baggage::default();
        b.insert_with_metadata("foo", StringValue::from("1"), "red;state=on;z=z=z");
        assert_eq!("foo=1;red;state=on;z=z=z", b.to_string());

        // "two members with properties"
        let mut b = Baggage::default();
        b.insert_with_metadata("foo", StringValue::from("1"), "red;state=on");
        b.insert_with_metadata("bar", StringValue::from("2"), "yellow");
        assert!(b.to_string().contains("bar=2;yellow"));
        assert!(b.to_string().contains("foo=1;red;state=on"));
    }
}
