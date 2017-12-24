// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{BTreeSet, BTreeMap};
use key_server_cluster::{Error, NodeId, SessionMeta};

/// Partial response action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobPartialResponseAction {
	/// Ignore this response.
	Ignore,
	/// Mark this response as reject.
	Reject,
	/// Accept this response.
	Accept,
}

/// Partial request action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobPartialRequestAction<PartialJobResponse> {
	/// Respond with reject.
	Reject(PartialJobResponse),
	/// Respond with this response.
	Respond(PartialJobResponse),
}

/// Job executor.
pub trait JobExecutor {
	type PartialJobRequest;
	type PartialJobResponse;
	type JobResponse;

	/// Prepare job request for given node.
	fn prepare_partial_request(&self, node: &NodeId, nodes: &BTreeSet<NodeId>) -> Result<Self::PartialJobRequest, Error>;
	/// Process partial request.
	fn process_partial_request(&mut self, partial_request: Self::PartialJobRequest) -> Result<JobPartialRequestAction<Self::PartialJobResponse>, Error>;
	/// Check partial response of given node.
	fn check_partial_response(&mut self, sender: &NodeId, partial_response: &Self::PartialJobResponse) -> Result<JobPartialResponseAction, Error>;
	/// Compute final job response.
	fn compute_response(&self, partial_responses: &BTreeMap<NodeId, Self::PartialJobResponse>) -> Result<Self::JobResponse, Error>;
}

/// Jobs transport.
pub trait JobTransport {
	type PartialJobRequest;
	type PartialJobResponse;

	/// Send partial request to given node.
	fn send_partial_request(&self, node: &NodeId, request: Self::PartialJobRequest) -> Result<(), Error>;
	/// Send partial request to given node.
	fn send_partial_response(&self, node: &NodeId, response: Self::PartialJobResponse) -> Result<(), Error>;
}

/// Current state of job session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobSessionState {
	/// Session is inactive.
	Inactive,
	/// Session is active.
	Active,
	/// Session is finished.
	Finished,
	/// Session has failed.
	Failed,
}

/// Basic request-response session on a set of nodes.
pub struct JobSession<Executor: JobExecutor, Transport> where Transport: JobTransport<PartialJobRequest = Executor::PartialJobRequest, PartialJobResponse = Executor::PartialJobResponse> {
	/// Session meta.
	meta: SessionMeta,
	/// Job executor.
	executor: Executor,
	/// Jobs transport.
	transport: Transport,
	/// Session data.
	data: JobSessionData<Executor::PartialJobResponse>,
}

/// Data of job session.
struct JobSessionData<PartialJobResponse> {
	/// Session state.
	state: JobSessionState,
	/// Mutable session data.
	active_data: Option<ActiveJobSessionData<PartialJobResponse>>,
}

/// Active job session data.
struct ActiveJobSessionData<PartialJobResponse> {
	/// Active partial requests.
	requests: BTreeSet<NodeId>,
	/// Rejects to partial requests.
	rejects: BTreeSet<NodeId>,
	/// Received partial responses.
	responses: BTreeMap<NodeId, PartialJobResponse>,
}

impl<Executor, Transport> JobSession<Executor, Transport> where Executor: JobExecutor, Transport: JobTransport<PartialJobRequest = Executor::PartialJobRequest, PartialJobResponse = Executor::PartialJobResponse> {
	/// Create new session.
	pub fn new(meta: SessionMeta, executor: Executor, transport: Transport) -> Self {
		JobSession {
			meta: meta,
			executor: executor,
			transport: transport,
			data: JobSessionData {
				state: JobSessionState::Inactive,
				active_data: None,
			},
		}
	}

	/// Get transport reference.
	#[cfg(test)]
	pub fn transport(&self) -> &Transport {
		&self.transport
	}

	/// Get mutable transport reference.
	pub fn transport_mut(&mut self) -> &mut Transport {
		&mut self.transport
	}

	/// Get executor reference.
	pub fn executor(&self) -> &Executor {
		&self.executor
	}

	/// Get mutable executor reference.
	pub fn executor_mut(&mut self) -> &mut Executor {
		&mut self.executor
	}

	/// Get job state.
	pub fn state(&self) -> JobSessionState {
		self.data.state
	}

	/// Get rejects.
	#[cfg(test)]
	pub fn rejects(&self) -> &BTreeSet<NodeId> {
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);

		&self.data.active_data.as_ref()
			.expect("rejects is only called on master nodes after initialization; on master nodes active_data is filled during initialization; qed")
			.rejects
	}

	/// Get active requests.
	pub fn requests(&self) -> &BTreeSet<NodeId> {
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);

		&self.data.active_data.as_ref()
			.expect("requests is only called on master nodes after initialization; on master nodes active_data is filled during initialization; qed")
			.requests
	}

	/// Get responses.
	pub fn responses(&self) -> &BTreeMap<NodeId, Executor::PartialJobResponse> {
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);

		&self.data.active_data.as_ref()
			.expect("responses is only called on master nodes after initialization; on master nodes active_data is filled during initialization; qed")
			.responses
	}

	/// Get job result.
	pub fn result(&self) -> Result<Executor::JobResponse, Error> {
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);

		if self.data.state != JobSessionState::Finished {
			return Err(Error::InvalidStateForRequest);
		}

		self.executor.compute_response(&self.data.active_data.as_ref()
			.expect("requests is only called on master nodes; on master nodes active_data is filled during initialization; qed")
			.responses)
	}

	/// Initialize.
	pub fn initialize(&mut self, nodes: BTreeSet<NodeId>) -> Result<(), Error> {		
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);

		if nodes.len() < self.meta.threshold + 1 {
			return Err(Error::ConsensusUnreachable);
		}

		if self.data.state != JobSessionState::Inactive {
			return Err(Error::InvalidStateForRequest);
		}

		// send requests to slave nodes
		let mut waits_for_self = false;
		let active_data = ActiveJobSessionData {
			requests: nodes,
			rejects: BTreeSet::new(),
			responses: BTreeMap::new(),
		};
		for node in &active_data.requests {
			if node != &self.meta.self_node_id {
				self.transport.send_partial_request(&node, self.executor.prepare_partial_request(node, &active_data.requests)?)?;
			} else {
				waits_for_self = true;
			}
		}

		// result from self
		let self_response = if waits_for_self {
			let partial_request = self.executor.prepare_partial_request(&self.meta.self_node_id, &active_data.requests)?;
			Some(self.executor.process_partial_request(partial_request)?)
		} else {
			None
		};

		// update state
		self.data.active_data = Some(active_data);
		self.data.state = JobSessionState::Active;

		// if we are waiting for response from self => do it
		if let Some(self_response) = self_response {
			let self_node_id = self.meta.self_node_id.clone();
			match self_response {
				JobPartialRequestAction::Respond(self_response) => self.on_partial_response(&self_node_id, self_response)?,
				JobPartialRequestAction::Reject(self_response) => self.on_partial_response(&self_node_id, self_response)?,
			}
		}

		Ok(())
	}

	/// When partial request is received by slave node.
	pub fn on_partial_request(&mut self, node: &NodeId, request: Executor::PartialJobRequest) -> Result<(), Error> {
		if node != &self.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if self.meta.self_node_id == self.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if self.data.state != JobSessionState::Inactive && self.data.state != JobSessionState::Finished {
			return Err(Error::InvalidStateForRequest);
		}

		let partial_response = match self.executor.process_partial_request(request)? {
			JobPartialRequestAction::Respond(partial_response) => {
				self.data.state = JobSessionState::Finished;
				partial_response
			},
			JobPartialRequestAction::Reject(partial_response) => {
				self.data.state = JobSessionState::Failed;
				partial_response
			},
		};
		self.transport.send_partial_response(node, partial_response)
	}

	/// When partial request is received by master node.
	pub fn on_partial_response(&mut self, node: &NodeId, response: Executor::PartialJobResponse) -> Result<(), Error> {
		if self.meta.self_node_id != self.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if self.data.state != JobSessionState::Active && self.data.state != JobSessionState::Finished {
			return Err(Error::InvalidStateForRequest);
		}

		let active_data = self.data.active_data.as_mut()
			.expect("on_partial_response is only called on master nodes; on master nodes active_data is filled during initialization; qed");
		if !active_data.requests.remove(node) {
			return Err(Error::InvalidNodeForRequest);
		}
		
		match self.executor.check_partial_response(node, &response)? {
			JobPartialResponseAction::Ignore => Ok(()),
			JobPartialResponseAction::Reject => {
				active_data.rejects.insert(node.clone());
				if active_data.requests.len() + active_data.responses.len() >= self.meta.threshold + 1 {
					return Ok(());
				}

				self.data.state = JobSessionState::Failed;
				Err(Error::ConsensusUnreachable)
			},
			JobPartialResponseAction::Accept => {
				active_data.responses.insert(node.clone(), response);
				if active_data.responses.len() < self.meta.threshold + 1 {
					return Ok(());
				}

				self.data.state = JobSessionState::Finished;
				Ok(())
			},
		}
	}

	/// When error from node is received.
	pub fn on_node_error(&mut self, node: &NodeId) -> Result<(), Error> {
		if self.meta.self_node_id != self.meta.master_node_id {
			if node != &self.meta.master_node_id {
				return Ok(());
			}

			self.data.state = JobSessionState::Failed;
			return Err(Error::ConsensusUnreachable);
		}

		if let Some(active_data) = self.data.active_data.as_mut() {
			if active_data.rejects.contains(node) {
				return Ok(());
			}
			if active_data.requests.remove(node) || active_data.responses.remove(node).is_some() {
				active_data.rejects.insert(node.clone());
				if self.data.state == JobSessionState::Finished && active_data.responses.len() < self.meta.threshold + 1 {
					self.data.state = JobSessionState::Active;
				}
				if active_data.requests.len() + active_data.responses.len() >= self.meta.threshold + 1 {
					return Ok(());
				}

				self.data.state = JobSessionState::Failed;
				return Err(Error::ConsensusUnreachable);
			}
		}

		Ok(())
	}

	/// When session timeouted.
	pub fn on_session_timeout(&mut self) -> Result<(), Error> {
		if self.data.state == JobSessionState::Finished || self.data.state == JobSessionState::Failed {
			return Ok(());
		}

		self.data.state = JobSessionState::Failed;
		Err(Error::ConsensusUnreachable)
	}
}


#[cfg(test)]
pub mod tests {
	use std::collections::{VecDeque, BTreeMap, BTreeSet};
	use parking_lot::Mutex;
	use ethkey::Public;
	use key_server_cluster::{Error, NodeId, SessionId, SessionMeta};
	use super::{JobPartialResponseAction, JobPartialRequestAction, JobExecutor, JobTransport, JobSession, JobSessionState};

	pub struct SquaredSumJobExecutor;

	impl JobExecutor for SquaredSumJobExecutor {
		type PartialJobRequest = u32;
		type PartialJobResponse = u32;
		type JobResponse = u32;

		fn prepare_partial_request(&self, _n: &NodeId, _nodes: &BTreeSet<NodeId>) -> Result<u32, Error> { Ok(2) }
		fn process_partial_request(&mut self, r: u32) -> Result<JobPartialRequestAction<u32>, Error> { if r <= 10 { Ok(JobPartialRequestAction::Respond(r * r)) } else { Err(Error::InvalidMessage) } }
		fn check_partial_response(&mut self, _s: &NodeId, r: &u32) -> Result<JobPartialResponseAction, Error> { if r % 2 == 0 { Ok(JobPartialResponseAction::Accept) } else { Ok(JobPartialResponseAction::Reject) } }
		fn compute_response(&self, r: &BTreeMap<NodeId, u32>) -> Result<u32, Error> { Ok(r.values().fold(0, |v1, v2| v1 + v2)) }
	}

	#[derive(Default)]
	pub struct DummyJobTransport<T, U> {
		pub requests: Mutex<VecDeque<(NodeId, T)>>,
		pub responses: Mutex<VecDeque<(NodeId, U)>>,
	}

	impl<T, U> DummyJobTransport<T, U> {
		pub fn response(&self) -> (NodeId, U) {
			self.responses.lock().pop_front().unwrap()
		}
	}

	impl<T, U> JobTransport for DummyJobTransport<T, U> {
		type PartialJobRequest = T;
		type PartialJobResponse = U;

		fn send_partial_request(&self, node: &NodeId, request: T) -> Result<(), Error> { self.requests.lock().push_back((node.clone(), request)); Ok(()) }
		fn send_partial_response(&self, node: &NodeId, response: U) -> Result<(), Error> { self.responses.lock().push_back((node.clone(), response)); Ok(()) }
	}

	pub fn make_master_session_meta(threshold: usize) -> SessionMeta {
		SessionMeta { id: SessionId::default(), master_node_id: NodeId::from(1), self_node_id: NodeId::from(1), threshold: threshold }
	}

	pub fn make_slave_session_meta(threshold: usize) -> SessionMeta {
		SessionMeta { id: SessionId::default(), master_node_id: NodeId::from(1), self_node_id: NodeId::from(2), threshold: threshold }
	}

	#[test]
	fn job_initialize_fails_if_not_inactive() {
		let mut job = JobSession::new(make_master_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1)].into_iter().collect()).unwrap();
		assert_eq!(job.initialize(vec![Public::from(1)].into_iter().collect()).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn job_initialization_leads_to_finish_if_single_node_is_required() {
		let mut job = JobSession::new(make_master_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Finished);
		assert_eq!(job.result(), Ok(4));
	}

	#[test]
	fn job_initialization_does_not_leads_to_finish_if_single_other_node_is_required() {
		let mut job = JobSession::new(make_master_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_request_fails_if_comes_from_non_master_node() {
		let mut job = JobSession::new(make_slave_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.on_partial_request(&NodeId::from(3), 2).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn job_request_fails_if_comes_to_master_node() {
		let mut job = JobSession::new(make_master_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.on_partial_request(&NodeId::from(1), 2).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn job_request_fails_if_comes_to_failed_state() {
		let mut job = JobSession::new(make_slave_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		job.on_session_timeout().unwrap_err();
		assert_eq!(job.on_partial_request(&NodeId::from(1), 2).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn job_request_succeeds_if_comes_to_finished_state() {
		let mut job = JobSession::new(make_slave_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		job.on_partial_request(&NodeId::from(1), 2).unwrap();
		assert_eq!(job.transport().response(), (NodeId::from(1), 4));
		assert_eq!(job.state(), JobSessionState::Finished);
		job.on_partial_request(&NodeId::from(1), 3).unwrap();
		assert_eq!(job.transport().response(), (NodeId::from(1), 9));
		assert_eq!(job.state(), JobSessionState::Finished);
	}

	#[test]
	fn job_response_fails_if_comes_to_slave_node() {
		let mut job = JobSession::new(make_slave_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.on_partial_response(&NodeId::from(1), 2).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn job_response_fails_if_comes_to_failed_state() {
		let mut job = JobSession::new(make_master_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(2)].into_iter().collect()).unwrap();
		job.on_session_timeout().unwrap_err();
		assert_eq!(job.on_partial_response(&NodeId::from(2), 2).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn job_response_fails_if_comes_from_unknown_node() {
		let mut job = JobSession::new(make_master_session_meta(0), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.on_partial_response(&NodeId::from(3), 2).unwrap_err(), Error::InvalidNodeForRequest);
	}

	#[test]
	fn job_response_leads_to_failure_if_too_few_nodes_left() {
		let mut job = JobSession::new(make_master_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		assert_eq!(job.on_partial_response(&NodeId::from(2), 3).unwrap_err(), Error::ConsensusUnreachable);
		assert_eq!(job.state(), JobSessionState::Failed);
	}

	#[test]
	fn job_response_succeeds() {
		let mut job = JobSession::new(make_master_session_meta(2), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2), Public::from(3)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_partial_response(&NodeId::from(2), 2).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_response_leads_to_finish() {
		let mut job = JobSession::new(make_master_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_partial_response(&NodeId::from(2), 2).unwrap();
		assert_eq!(job.state(), JobSessionState::Finished);
	}

	#[test]
	fn job_node_error_ignored_when_slave_disconnects_from_slave() {
		let mut job = JobSession::new(make_slave_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.state(), JobSessionState::Inactive);
		job.on_node_error(&NodeId::from(3)).unwrap();
		assert_eq!(job.state(), JobSessionState::Inactive);
	}

	#[test]
	fn job_node_error_leads_to_fail_when_slave_disconnects_from_master() {
		let mut job = JobSession::new(make_slave_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.state(), JobSessionState::Inactive);
		assert_eq!(job.on_node_error(&NodeId::from(1)).unwrap_err(), Error::ConsensusUnreachable);
		assert_eq!(job.state(), JobSessionState::Failed);
	}

	#[test]
	fn job_node_error_ignored_when_disconnects_from_rejected() {
		let mut job = JobSession::new(make_master_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2), Public::from(3)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_partial_response(&NodeId::from(2), 3).unwrap();
		job.on_node_error(&NodeId::from(2)).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_node_error_ignored_when_disconnects_from_unknown() {
		let mut job = JobSession::new(make_master_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_node_error(&NodeId::from(3)).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_node_error_ignored_when_disconnects_from_requested_and_enough_nodes_left() {
		let mut job = JobSession::new(make_master_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2), Public::from(3)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_node_error(&NodeId::from(3)).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_node_error_leads_to_fail_when_disconnects_from_requested_and_not_enough_nodes_left() {
		let mut job = JobSession::new(make_master_session_meta(1), SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		assert_eq!(job.on_node_error(&NodeId::from(2)).unwrap_err(), Error::ConsensusUnreachable);
		assert_eq!(job.state(), JobSessionState::Failed);
	}
}
