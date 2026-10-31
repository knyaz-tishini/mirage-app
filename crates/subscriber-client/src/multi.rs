//! Several configurations in one process.
//!
//! One customer may hold several CONFIGURATIONS against one box: each with
//! its own key, its own address inside the plane and its own local surface.
//! The core holds them side by side - one session each, one SOCKS5 port
//! each - and nothing is shared but the process.
//!
//! Every configuration is a whole [`SubscriberClient`]: its own supervisor,
//! its own listener, its own status file. A configuration that goes away
//! takes only its own surfaces with it.

use std::collections::BTreeMap;

use crate::artefact::DeliveryArtefact;
use crate::assembly::{SubscriberClient, SubscriberClientError, SubscriberClientOptions};

/// The clients of one process, one per configuration.
pub struct ClientSet {
    /// The clients, by label.
    clients: BTreeMap<String, SubscriberClient>,
}

impl Default for ClientSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientSet {
    /// An empty set.
    pub fn new() -> Self {
        ClientSet { clients: BTreeMap::new() }
    }

    /// Add a configuration and stand its local surface up.
    ///
    /// A configuration with a label that is already taken replaces the old
    /// one: the old client is shut down first, so its listener and its
    /// session go away before the new one takes the label.
    pub async fn add(
        &mut self,
        label: &str,
        artefact: &DeliveryArtefact,
        options: SubscriberClientOptions,
    ) -> Result<(), SubscriberClientError> {
        if let Some(old) = self.clients.remove(label) {
            old.shutdown();
        }
        let client = SubscriberClient::build_from_delivery(artefact, options).await?;
        self.clients.insert(label.to_owned(), client);
        Ok(())
    }

    /// The client of one configuration.
    pub fn get(&self, label: &str) -> Option<&SubscriberClient> {
        self.clients.get(label)
    }

    /// The labels of the configurations, in order.
    pub fn labels(&self) -> Vec<&str> {
        self.clients.keys().map(String::as_str).collect()
    }

    /// How many configurations this set serves.
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Whether this set serves nothing.
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Take one configuration away. Its client is dropped, which shuts its
    /// listener and its session down.
    pub fn remove(&mut self, label: &str) -> Option<SubscriberClient> {
        self.clients.remove(label)
    }

    /// Shut every configuration down.
    pub fn shutdown(&mut self) {
        for (_, client) in std::mem::take(&mut self.clients) {
            client.shutdown();
        }
    }
}

impl Drop for ClientSet {
    /// Nothing of a configuration may outlive the set that owns it.
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_set_serves_nothing() {
        let set = ClientSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert!(set.labels().is_empty());
        assert!(set.get("consumer-1").is_none());
    }
}
