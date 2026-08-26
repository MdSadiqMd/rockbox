//! Internally-tagged enum (`cmd` discriminator) so Elixir Msgpax `%{"cmd" =>
//! "execute", ...}` maps directly without an outer wrapper.

pub use crate::settings::FileEntry;
use crate::settings::Settings;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// One-shot or initial run. Carries the full frozen `Settings`.
    Execute(Box<Settings>),

    /// Session-mode REPL cell. The session VM was already booted by an earlier
    /// `execute` with `mode=session`; this just hands more code in.
    ExecCell {
        id: String,
        session_id: String,
        code: String,
        #[serde(default)]
        files: Vec<FileEntry>,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        wall_ms: Option<u64>,
    },

    /// RL-mode step. Action bytes flow through; observation comes back via [`crate::Response::RlStep`].
    RlStep {
        id: String,
        episode_id: String,
        #[serde(with = "serde_bytes")]
        action: Vec<u8>,
    },

    /// RL-mode BATCHED steps (EnvPool-style): pipeline N actions over the
    /// worker's fd-3 protocol pipe in ONE engine↔worker round trip sequence
    /// and return every tick in a single control-channel frame. Cuts
    /// per-step orchestrator overhead (HTTP + Port + GenServer hops) by N×
    /// for rollout loops that know their action sequences ahead of time.
    /// Steps run strictly in order; the batch stops early if an env raises.
    RlSteps {
        id: String,
        episode_id: String,
        #[serde(deserialize_with = "bytes_or_str_frames")]
        actions: Vec<Vec<u8>>,
    },

    /// Forward additional stdin to the running child.
    Stdin {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },

    /// Interrupt the currently-running cell/step (SIGINT to PID 1).
    Interrupt { id: String },

    /// LSP request relay.
    Lsp(LspParams),

    /// Graceful shutdown - engine flushes pending events, then exits.
    Shutdown,
}

/// Deserialize a list of byte frames that may arrive as msgpack `bin` or
/// `str` per element (Msgpax packs Elixir binaries as str by default).
fn bytes_or_str_frames<'de, D>(d: D) -> Result<Vec<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Frame(#[serde(with = "serde_bytes")] Vec<u8>);

    let frames = Vec::<Frame>::deserialize(d)?;
    Ok(frames.into_iter().map(|f| f.0).collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspParams {
    pub method: String,
    /// Opaque msgpack bytes; the engine forwards them verbatim to the
    /// language server hosted inside the sandbox.
    #[serde(default, with = "serde_bytes")]
    pub params: Vec<u8>,
    pub req_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::Response;

    // The Elixir orchestrator sends internally-tagged msgpack maps (Msgpax
    // packs atom keys as strings). Encode the exact same shape with rmp-serde
    // and assert Command::RlSteps decodes from it.
    #[test]
    fn rl_steps_decodes_from_elixir_wire_shape() {
        #[derive(serde::Serialize)]
        struct ElixirShape<'a> {
            cmd: &'a str,
            id: &'a str,
            episode_id: &'a str,
            actions: Vec<Vec<u8>>,
        }

        let wire = rmp_serde::to_vec_named(&ElixirShape {
            cmd: "rl_steps",
            id: "req_abc",
            episode_id: "ep_123",
            actions: vec![vec![7], vec![1, 2, 255], vec![]],
        })
        .unwrap();

        match rmp_serde::from_slice::<Command>(&wire).expect("decode") {
            Command::RlSteps {
                id,
                episode_id,
                actions,
            } => {
                assert_eq!(id, "req_abc");
                assert_eq!(episode_id, "ep_123");
                assert_eq!(actions.len(), 3);
                assert_eq!(actions[1], vec![1, 2, 255]);
                assert!(actions[2].is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rl_steps_decodes_str_encoded_actions() {
        // Msgpax packs Elixir binaries as msgpack *str* unless explicitly
        // wrapped; the decoder must accept both shapes per element.
        #[derive(serde::Serialize)]
        struct StrFrame<'a>(&'a str);

        #[derive(serde::Serialize)]
        struct ElixirStrShape<'a> {
            cmd: &'a str,
            id: &'a str,
            episode_id: &'a str,
            actions: Vec<StrFrame<'a>>,
        }

        let wire = rmp_serde::to_vec_named(&ElixirStrShape {
            cmd: "rl_steps",
            id: "req_abc",
            episode_id: "ep_123",
            actions: vec![StrFrame("\u{7}"), StrFrame("\u{1}\u{2}")],
        })
        .unwrap();

        match rmp_serde::from_slice::<Command>(&wire).expect("decode") {
            Command::RlSteps { actions, .. } => {
                assert_eq!(actions, vec![vec![7], vec![1, 2]]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rl_steps_response_round_trips() {
        use crate::{EpisodeMetrics, RlTick};
        let resp = Response::RlSteps {
            request_id: "r".into(),
            episode_id: "e".into(),
            ticks: vec![RlTick {
                request_id: "t".into(),
                observation: vec![9],
                reward: 1.5,
                done: true,
                terminated: false,
                truncated: true,
                info: Default::default(),
                obs_meta: None,
            }],
            metrics: EpisodeMetrics {
                steps: 10,
                reward_sum: 4.2,
                elapsed_ms: 900,
            },
        };
        let bytes = rmp_serde::to_vec_named(&resp).unwrap();
        let back: Response = rmp_serde::from_slice(&bytes).unwrap();
        match back {
            Response::RlSteps { ticks, metrics, .. } => {
                assert_eq!(ticks.len(), 1);
                assert!(ticks[0].truncated);
                assert!(!ticks[0].terminated);
                assert!((metrics.reward_sum - 4.2).abs() < 1e-9);
                assert_eq!(metrics.steps, 10);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
