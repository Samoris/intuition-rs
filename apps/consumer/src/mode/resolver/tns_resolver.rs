use crate::{
    error::ConsumerError,
    mode::{
        ipfs_upload::types::IpfsUploadMessage,
        resolver::ens_resolver::Ens,
        types::ResolverConsumerContext,
    },
};
use alloy::{
    primitives::{Address, FixedBytes, keccak256},
    providers::DynProvider,
    sol,
};
use alloy_network::Ethereum;
use tracing::{debug, warn};

sol! {
    #[sol(rpc)]
    interface TNSRegistry {
        function resolver(bytes32 node) external view returns (address);
        function owner(bytes32 node) external view returns (address);
    }

    #[sol(rpc)]
    interface TNSResolver {
        function addr(bytes32 node) external view returns (address);
        function name(bytes32 node) external view returns (string);
        function text(bytes32 node, string key) external view returns (string);
        function contenthash(bytes32 node) external view returns (bytes);
    }
}

pub const TNS_REGISTRY_ADDRESS: &str = "0x3220B4EDbA3a1661F02f1D8D241DBF55EDcDa09e";
pub const INTUITION_RPC_URL: &str = "https://intuition.calderachain.xyz";

pub struct Tns;

impl Tns {
    /// Resolves the TNS primary name and avatar for an address.
    ///
    /// Returns `Ok(Ens { name: None, image: None })` if no name is set or if the
    /// lookup fails for any reason. Never panics or blocks message processing.
    pub async fn get_tns(
        address: Address,
        consumer_context: &ResolverConsumerContext,
    ) -> Result<Ens, ConsumerError> {
        let name = Self::get_tns_name(address, &consumer_context.tns_client).await;
        let mut image = None;
        if let Some(name_str) = &name {
            image = Self::get_tns_avatar(name_str, consumer_context).await?;
        }
        Ok(Ens { name, image })
    }

    /// Fetches the TNS avatar text record and queues it for IPFS upload.
    async fn get_tns_avatar(
        name: &str,
        consumer_context: &ResolverConsumerContext,
    ) -> Result<Option<String>, ConsumerError> {
        match Self::get_text_record(name, "avatar", &consumer_context.tns_client).await {
            Some(url) => {
                debug!("Sending TNS avatar to IPFS upload consumer: {}", url);
                consumer_context
                    .client
                    .send_message(
                        serde_json::to_string(&IpfsUploadMessage { image: url.clone() })?,
                        None,
                    )
                    .await?;
                Ok(Some(url))
            }
            None => Ok(None),
        }
    }

    /// Reverse-resolves an address to its primary TNS name.
    ///
    /// Returns `None` if no name is set or if the lookup fails for any reason
    /// (network error, missing resolver, etc.). Never panics or blocks message processing.
    async fn get_tns_name(
        address: Address,
        registry: &TNSRegistry::TNSRegistryInstance<DynProvider, Ethereum>,
    ) -> Option<String> {
        debug!("Getting TNS name for {} via TNS Registry", address);

        let reverse_name = Self::prepare_reverse_name(address);
        let node = Self::namehash(&reverse_name);
        let node_bytes = FixedBytes::from_slice(node.as_slice());

        let resolver = Self::get_resolver(node_bytes, registry).await?;

        match resolver.name(node_bytes).call().await {
            Ok(name) if !name.is_empty() => {
                debug!("Resolved TNS name for {}: {}", address, name);
                Some(name)
            }
            Ok(_) => {
                debug!("No TNS primary name set for {}", address);
                None
            }
            Err(e) => {
                warn!(
                    "TNS name lookup failed for {} (non-fatal, returning None): {}",
                    address, e
                );
                None
            }
        }
    }

    fn namehash(name: &str) -> Vec<u8> {
        if name.is_empty() {
            return vec![0u8; 32];
        }
        let mut hash = vec![0u8; 32];
        for label in name.rsplit('.') {
            hash.append(&mut keccak256(label.as_bytes()).to_vec());
            hash = keccak256(hash.as_slice()).to_vec();
        }
        hash
    }

    fn ensure_trust_suffix(name: &str) -> String {
        if name.ends_with(".trust") {
            name.to_string()
        } else {
            format!("{}.trust", name)
        }
    }

    /// Looks up the resolver contract for a given node hash.
    /// Returns `None` if no resolver is set or if the call fails.
    async fn get_resolver(
        node_bytes: FixedBytes<32>,
        registry: &TNSRegistry::TNSRegistryInstance<DynProvider, Ethereum>,
    ) -> Option<TNSResolver::TNSResolverInstance<DynProvider, Ethereum>> {
        match registry.resolver(node_bytes).call().await {
            Ok(addr) if addr != Address::ZERO => Some(TNSResolver::TNSResolverInstance::new(
                addr,
                registry.provider().clone(),
            )),
            Ok(_) => None,
            Err(e) => {
                warn!(
                    "TNS registry resolver lookup failed (non-fatal, returning None): {}",
                    e
                );
                None
            }
        }
    }

    fn node_for_name(name: &str) -> FixedBytes<32> {
        let full_name = Self::ensure_trust_suffix(name);
        let node = Self::namehash(&full_name);
        FixedBytes::from_slice(node.as_slice())
    }

    async fn get_text_record(
        name: &str,
        key: &str,
        registry: &TNSRegistry::TNSRegistryInstance<DynProvider, Ethereum>,
    ) -> Option<String> {
        let node_bytes = Self::node_for_name(name);
        let resolver = Self::get_resolver(node_bytes, registry).await?;
        resolver
            .text(node_bytes, key.to_string())
            .call()
            .await
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn prepare_reverse_name(address: Address) -> String {
        let addr_str = address.to_string().to_lowercase();
        format!("{}.addr.reverse", addr_str.trim_start_matches("0x"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_namehash_empty() {
        assert_eq!(Tns::namehash(""), vec![0u8; 32]);
    }

    #[test]
    fn test_namehash_trust() {
        let hash = Tns::namehash("trust");
        assert_eq!(hash.len(), 32);
        assert_ne!(hash, vec![0u8; 32]);
    }

    #[test]
    fn test_namehash_alice_trust() {
        let hash = Tns::namehash("alice.trust");
        assert_eq!(hash.len(), 32);
        assert_ne!(hash, vec![0u8; 32]);
        assert_ne!(hash, Tns::namehash("trust"));
    }

    #[test]
    fn test_namehash_consistency() {
        let hash1 = Tns::namehash("alice.trust");
        let hash2 = Tns::namehash("alice.trust");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_namehash_different_names() {
        let hash_alice = Tns::namehash("alice.trust");
        let hash_bob = Tns::namehash("bob.trust");
        assert_ne!(hash_alice, hash_bob);
    }

    #[test]
    fn test_prepare_reverse_name() {
        let address: Address = "0xf1016a7fe89eb9d244c3bfb270071b24619e36c6"
            .parse()
            .unwrap();
        let reverse = Tns::prepare_reverse_name(address);
        assert_eq!(
            reverse,
            "f1016a7fe89eb9d244c3bfb270071b24619e36c6.addr.reverse"
        );
    }

    #[test]
    fn test_ensure_trust_suffix() {
        assert_eq!(Tns::ensure_trust_suffix("alice"), "alice.trust");
        assert_eq!(Tns::ensure_trust_suffix("alice.trust"), "alice.trust");
        assert_eq!(Tns::ensure_trust_suffix("bob.sub"), "bob.sub.trust");
    }

    #[tokio::test]
    #[ignore = "Requires live Intuition RPC - run with --ignored"]
    async fn test_get_tns_name_live() {
        use alloy::providers::ProviderBuilder;

        let provider = ProviderBuilder::new()
            .connect_http(INTUITION_RPC_URL.parse().unwrap());
        let dyn_provider = DynProvider::new(provider);
        let registry = TNSRegistry::TNSRegistryInstance::new(
            TNS_REGISTRY_ADDRESS.parse::<Address>().unwrap(),
            dyn_provider,
        );

        let address: Address = "0xf1016a7Fe89EB9D244c3bfB270071b24619e36C6"
            .parse()
            .unwrap();
        let name = Tns::get_tns_name(address, &registry).await;
        println!("TNS name for {address}: {name:?}");
    }

    #[tokio::test]
    #[ignore = "Requires live Intuition RPC - run with --ignored"]
    async fn test_avatar_text_record_live() {
        use alloy::providers::ProviderBuilder;

        let provider = ProviderBuilder::new()
            .connect_http(INTUITION_RPC_URL.parse().unwrap());
        let dyn_provider = DynProvider::new(provider);
        let registry = TNSRegistry::TNSRegistryInstance::new(
            TNS_REGISTRY_ADDRESS.parse::<Address>().unwrap(),
            dyn_provider,
        );

        let name = "sammy.trust";
        let avatar = Tns::get_text_record(name, "avatar", &registry).await;
        println!("TNS avatar for {name}: {avatar:?}");

        let url = Tns::get_text_record(name, "url", &registry).await;
        println!("TNS url for {name}: {url:?}");
    }
}
