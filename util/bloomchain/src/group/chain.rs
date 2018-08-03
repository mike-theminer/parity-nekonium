use std::collections::HashMap;
use std::ops::Range;
use bloom::Bloom;
use chain::BloomChain;
use config::Config;
use number::Number;
use filter::Filter;
use position::Position as BloomPosition;
use super::{GroupDatabaseBridge, BloomGroupDatabase, BloomGroup, GroupPosition};
use super::position::Manager as PositionManager;

/// Performs all bloom database operations using `BloomGroup`s.
pub struct BloomGroupChain<'a> {
	config: Config,
	db: &'a BloomGroupDatabase,
	bridge: GroupDatabaseBridge<'a>,
}

impl<'a> BloomGroupChain<'a> {
	pub fn new(config: Config, db: &'a BloomGroupDatabase) -> Self {
		let bridge = GroupDatabaseBridge::new(config, db);

		BloomGroupChain {
			config: config,
			db: db,
			bridge: bridge,
		}
	}

	fn group_blooms(&self, blooms: HashMap<BloomPosition, Bloom>) -> HashMap<GroupPosition, BloomGroup> {
		let positioner = PositionManager::new(self.config.elements_per_index);
		blooms.into_iter()
			.fold(HashMap::new(), | mut acc, (position, bloom) | {
				{
					let position = positioner.position(&position);
					let group = acc
						.entry(position.group.clone())
						.or_insert_with(|| self.db
							.blooms_at(&position.group)
							.unwrap_or_else(|| BloomGroup::new(self.config.elements_per_index))
						);
					assert_eq!(self.config.elements_per_index, group.blooms.len());
					group.blooms[position.number] = bloom;
				}
				acc
			})
	}

	pub fn insert(&self, number: Number, bloom: Bloom) -> HashMap<GroupPosition, BloomGroup> {
		let bloom_chain = BloomChain::new(self.config, &self.bridge);
		let modified_blooms = bloom_chain.insert(number, bloom);
		self.group_blooms(modified_blooms)
	}

	pub fn replace(&self, range: &Range<Number>, blooms: Vec<Bloom>) -> HashMap<GroupPosition, BloomGroup> {
		let bloom_chain = BloomChain::new(self.config, &self.bridge);
		let modified_blooms = bloom_chain.replace(range, blooms);
		self.group_blooms(modified_blooms)
	}

	pub fn with_bloom(&self, range: &Range<Number>, bloom: &Bloom) -> Vec<Number> {
		let bloom_chain = BloomChain::new(self.config, &self.bridge);
		bloom_chain.with_bloom(range, bloom)
	}

	pub fn filter(&self, filter: &Filter) -> Vec<Number> {
		let bloom_chain = BloomChain::new(self.config, &self.bridge);
		bloom_chain.filter(filter)
	}
}
