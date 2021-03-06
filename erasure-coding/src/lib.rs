// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! As part of Polkadot's availability system, certain pieces of data
//! for each block are required to be kept available.
//!
//! The way we accomplish this is by erasure coding the data into n pieces
//! and constructing a merkle root of the data.
//!
//! Each of n validators stores their piece of data. We assume n=3f+k, k < 3.
//! f is the maximum number of faulty vaildators in the system.
//! The data is coded so any f+1 chunks can be used to reconstruct the full data.

extern crate polkadot_primitives as primitives;
extern crate reed_solomon_erasure as reed_solomon;
extern crate parity_codec as codec;
extern crate substrate_primitives;
extern crate substrate_trie as trie;

use codec::{Encode, Decode};
use reed_solomon::galois_16::{self, ReedSolomon};
use primitives::{Hash as H256, BlakeTwo256, HashT};
use primitives::parachain::{BlockData, Extrinsic};
use substrate_primitives::Blake2Hasher;
use trie::{MemoryDB, Trie, TrieMut, TrieDB, TrieDBMut};

use self::wrapped_shard::WrappedShard;

mod wrapped_shard;

// we are limited to the field order of GF(2^16), which is 65536
const MAX_VALIDATORS: usize = <galois_16::Field as reed_solomon::Field>::ORDER;

/// Errors in erasure coding.
#[derive(Debug, Clone)]
pub enum Error {
	/// Returned when there are too many validators.
	TooManyValidators,
	/// Cannot encode something for no validators
	EmptyValidators,
	/// Cannot reconstruct: wrong number of validators.
	WrongValidatorCount,
	/// Not enough chunks present.
	NotEnoughChunks,
	/// Too many chunks present.
	TooManyChunks,
	/// Chunks not of uniform length or the chunks are empty.
	NonUniformChunks,
	/// An uneven byte-length of a shard is not valid for GF(2^16) encoding.
	UnevenLength,
	/// Chunk index out of bounds.
	ChunkIndexOutOfBounds(usize, usize),
	/// Bad payload in reconstructed bytes.
	BadPayload,
	/// Invalid branch proof.
	InvalidBranchProof,
	/// Branch out of bounds.
	BranchOutOfBounds,
}

struct CodeParams {
	data_shards: usize,
	parity_shards: usize,
}

impl CodeParams {
	// the shard length needed for a payload with initial size `base_len`.
	fn shard_len(&self, base_len: usize) -> usize {
		(base_len / self.data_shards) + (base_len % self.data_shards)
	}

	fn make_shards_for(&self, payload: &[u8]) -> Vec<WrappedShard> {
		let shard_len = self.shard_len(payload.len());
		let mut shards = vec![
			WrappedShard::new(vec![0; shard_len + 4]);
			self.data_shards + self.parity_shards
		];

		for (data_chunk, blank_shard) in payload.chunks(shard_len).zip(&mut shards) {
			let blank_shard: &mut [u8] = blank_shard.as_mut();
			let (len_slice, blank_shard) = blank_shard.split_at_mut(4);
			let len = ::std::cmp::min(data_chunk.len(), blank_shard.len());

			// prepend the length to each data shard. this will tell us how much
			// we need to read.
			//
			// this is necessary because we are doing RS encoding with 16-bit words,
			// but the payload is a byte-slice. We need to know how much data
			// to read from each shard when reconstructing.
			//
			// TODO: could be done more efficiently by pushing extra bytes onto the
			// end. https://github.com/paritytech/polkadot/issues/88
			(len as u32).using_encoded(|s| {
				len_slice.copy_from_slice(s)
			});

			// fill the empty shards with the corresponding piece of the payload,
			// zero-padded to fit in the shards.
			blank_shard[..len].copy_from_slice(&data_chunk[..len]);
		}

		shards
	}

	// make a reed-solomon instance.
	fn make_encoder(&self) -> ReedSolomon {
		ReedSolomon::new(self.data_shards, self.parity_shards)
			.expect("this struct is not created with invalid shard number; qed")
	}
}

fn code_params(n_validators: usize) -> Result<CodeParams, Error> {
	if n_validators > MAX_VALIDATORS { return Err(Error::TooManyValidators) }
	if n_validators == 0 { return Err(Error::EmptyValidators) }

	let n_faulty = n_validators.saturating_sub(1) / 3;
	let n_good = n_validators - n_faulty;

	Ok(CodeParams {
		data_shards: n_faulty + 1,
		parity_shards: n_good - 1,
	})
}

/// Obtain erasure-coded chunks, one for each validator.
///
/// Works only up to 256 validators, and `n_validators` must be non-zero.
pub fn obtain_chunks(n_validators: usize, block_data: &BlockData, extrinsic: &Extrinsic)
	-> Result<Vec<Vec<u8>>, Error>
{
	let params  = code_params(n_validators)?;
	let encoded = (block_data, extrinsic).encode();

	if encoded.is_empty() {
		return Err(Error::BadPayload);
	}

	let mut shards = params.make_shards_for(&encoded[..]);

	params.make_encoder().encode(&mut shards[..])
		.expect("Payload non-empty, shard sizes are uniform, and validator numbers checked; qed");

	Ok(shards.into_iter().map(|w| w.into_inner()).collect())
}

/// Reconstruct the block data from a set of chunks.
///
/// Provide an iterator containing chunk data and the corresponding index.
/// The indices of the present chunks must be indicated. If too few chunks
/// are provided, recovery is not possible.
///
/// Works only up to 256 validators, and `n_validators` must be non-zero.
pub fn reconstruct<'a, I: 'a>(n_validators: usize, chunks: I)
	-> Result<(BlockData, Extrinsic), Error>
	where I: IntoIterator<Item=(&'a [u8], usize)>
{
	let params = code_params(n_validators)?;
	let mut shards: Vec<Option<WrappedShard>> = vec![None; n_validators];
	let mut shard_len = None;
	for (chunk_data, chunk_idx) in chunks.into_iter().take(n_validators) {
		if chunk_idx >= n_validators {
			return Err(Error::ChunkIndexOutOfBounds(chunk_idx, n_validators));
		}

		let shard_len = shard_len.get_or_insert_with(|| chunk_data.len());

		if *shard_len % 2 != 0 {
			return Err(Error::UnevenLength);
		}

		if *shard_len != chunk_data.len() || *shard_len == 0 {
			return Err(Error::NonUniformChunks);
		}

		shards[chunk_idx] = Some(WrappedShard::new(chunk_data.to_vec()));
	}

	if let Err(e) = params.make_encoder().reconstruct(&mut shards[..]) {
		match e {
			reed_solomon::Error::TooFewShardsPresent => Err(Error::NotEnoughChunks)?,
			reed_solomon::Error::InvalidShardFlags => Err(Error::WrongValidatorCount)?,
			reed_solomon::Error::TooManyShards => Err(Error::TooManyChunks)?,
			reed_solomon::Error::EmptyShard => panic!("chunks are all non-empty; this is checked above; qed"),
			reed_solomon::Error::IncorrectShardSize => panic!("chunks are all same len; this is checked above; qed"),
			_ => panic!("reed_solomon encoder returns no more variants for this function; qed"),
		}
	}

	// lazily decode from the data shards.
	Decode::decode(&mut ShardInput {
		shards: shards.iter()
			.map(|x| x.as_ref())
			.take(params.data_shards)
			.map(|x| x.expect("all data shards have been recovered; qed"))
			.filter_map(|x| {
				let mut s: &[u8] = x.as_ref();
				let data_len = u32::decode(&mut s)? as usize;

				// NOTE: s has been mutated to point forward by `decode`.
				if s.len() < data_len {
					None
				} else {
					Some(&s[..data_len])
				}
			}),
		cur_shard: None,
	}).ok_or_else(|| Error::BadPayload)
}

/// An iterator that yields merkle branches and chunk data for all chunks to
/// be sent to other validators.
pub struct Branches<'a> {
	trie_storage: MemoryDB<Blake2Hasher>,
	root: H256,
	chunks: Vec<&'a [u8]>,
	current_pos: usize,
}

impl<'a> Branches<'a> {
	/// Get the trie root.
	pub fn root(&self) -> H256 { self.root.clone() }
}

impl<'a> Iterator for Branches<'a> {
	type Item = (Vec<Vec<u8>>, &'a [u8]);

	fn next(&mut self) -> Option<Self::Item> {
		use trie::Recorder;

		let trie = TrieDB::new(&self.trie_storage, &self.root)
			.expect("`Branches` is only created with a valid memorydb that contains all nodes for the trie with given root; qed");

		let mut recorder = Recorder::new();
		let res = (self.current_pos as u32).using_encoded(|s|
			trie.get_with(s, &mut recorder)
		);

		match res.expect("all nodes in trie present; qed") {
			Some(_) => {
				let nodes = recorder.drain().into_iter().map(|r| r.data).collect();
				let chunk = &self.chunks.get(self.current_pos)
					.expect("there is a one-to-one mapping of chunks to valid merkle branches; qed");

				self.current_pos += 1;
				Some((nodes, chunk))
			}
			None => None,
		}
	}
}

/// Construct a trie from chunks of an erasure-coded value. This returns the root hash and an
/// iterator of merkle proofs, one for each validator.
pub fn branches<'a>(chunks: Vec<&'a [u8]>) -> Branches<'a> {
	let mut trie_storage: MemoryDB<Blake2Hasher> = MemoryDB::default();
	let mut root = H256::default();

	// construct trie mapping each chunk's index to its hash.
	{
		let mut trie = TrieDBMut::new(&mut trie_storage, &mut root);
		for (i, &chunk) in chunks.iter().enumerate() {
			(i as u32).using_encoded(|encoded_index| {
				let chunk_hash = BlakeTwo256::hash(chunk);
				trie.insert(encoded_index, chunk_hash.as_ref())
					.expect("a fresh trie stored in memory cannot have errors loading nodes; qed");
			})
		}
	}

	Branches {
		trie_storage,
		root,
		chunks,
		current_pos: 0,
	}
}

/// Verify a markle branch, yielding the chunk hash meant to be present at that
/// index.
pub fn branch_hash(root: &H256, branch_nodes: &[Vec<u8>], index: usize) -> Result<H256, Error> {
	let mut trie_storage: MemoryDB<Blake2Hasher> = MemoryDB::default();
	for node in branch_nodes.iter() {
		(&mut trie_storage as &mut trie::HashDB<_>).insert(node.as_slice());
	}

	let trie = TrieDB::new(&trie_storage, &root).map_err(|_| Error::InvalidBranchProof)?;
	let res = (index as u32).using_encoded(|key|
		trie.get_with(key, |raw_hash: &[u8]| H256::decode(&mut &raw_hash[..]))
	);

	match res {
		Ok(Some(Some(hash))) => Ok(hash),
		Ok(Some(None)) => Err(Error::InvalidBranchProof), // hash failed to decode
		Ok(None) => Err(Error::BranchOutOfBounds),
		Err(_) => Err(Error::InvalidBranchProof),
	}
}

// input for `parity_codec` which draws data from the data shards
struct ShardInput<'a, I> {
	shards: I,
	cur_shard: Option<(&'a [u8], usize)>,
}

impl<'a, I: Iterator<Item=&'a [u8]>> codec::Input for ShardInput<'a, I> {
	fn read(&mut self, into: &mut [u8]) -> usize {
		let mut read_bytes = 0;

		loop {
			if read_bytes == into.len() { break }

			let cur_shard = self.cur_shard.take().or_else(|| self.shards.next().map(|s| (s, 0)));
			let (active_shard, mut in_shard) = match cur_shard {
				Some((s, i)) => (s, i),
				None => break,
			};

			if in_shard >= active_shard.len() {
				continue;
			}

			let remaining_len_out = into.len() - read_bytes;
			let remaining_len_shard = active_shard.len() - in_shard;

			let write_len = std::cmp::min(remaining_len_out, remaining_len_shard);
			into[read_bytes..][..write_len]
				.copy_from_slice(&active_shard[in_shard..][..write_len]);

			in_shard += write_len;
			read_bytes += write_len;
			self.cur_shard = Some((active_shard, in_shard))
		}

		read_bytes
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn field_order_is_right_size() {
		assert_eq!(MAX_VALIDATORS, 65536);
	}

    #[test]
	fn round_trip_block_data() {
		let block_data = BlockData((0..255).collect());
		let ex = Extrinsic { outgoing_messages: Vec::new() };
		let chunks = obtain_chunks(
			10,
			&block_data,
			&ex,
		).unwrap();

		assert_eq!(chunks.len(), 10);

		// any 4 chunks should work.
		let reconstructed = reconstruct(
			10,
			[
				(&*chunks[1], 1),
				(&*chunks[4], 4),
				(&*chunks[6], 6),
				(&*chunks[9], 9),
			].iter().cloned(),
		).unwrap();

		assert_eq!(reconstructed, (block_data, ex));
	}

	#[test]
	fn construct_valid_branches() {
		let block_data = BlockData(vec![2; 256]);
		let chunks = obtain_chunks(
			10,
			&block_data,
			&Extrinsic { outgoing_messages: Vec::new() },
		).unwrap();
		let chunks: Vec<_> = chunks.iter().map(|c| &c[..]).collect();

		assert_eq!(chunks.len(), 10);

		let branches = branches(chunks.clone());
		let root = branches.root();

		let proofs: Vec<_> = branches.map(|(proof, _)| proof).collect();

		assert_eq!(proofs.len(), 10);

		for (i, proof) in proofs.into_iter().enumerate() {
			assert_eq!(branch_hash(&root, &proof, i).unwrap(), BlakeTwo256::hash(chunks[i]));
		}
	}
}
