use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Running,
    Idle,
    Exited,
}

impl AgentStatus {
    /// Returns (r, g, b) for the traffic light dot.
    pub fn color(&self) -> (f64, f64, f64) {
        match self {
            AgentStatus::Running => (0.298, 0.686, 0.314), // green
            AgentStatus::Idle => (1.0, 0.757, 0.027),      // amber
            AgentStatus::Exited => (0.898, 0.224, 0.208),  // red
        }
    }
}
