use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipelineLifecycleState {
    VoidPending,
    Null,
    Ready,
    Paused,
    Playing,
}

pub(crate) fn require_settled_state(
    current: PipelineLifecycleState,
    pending: PipelineLifecycleState,
    expected: PipelineLifecycleState,
    description: &str,
) -> Result<()> {
    if current != expected || pending != PipelineLifecycleState::VoidPending {
        return Err(Error::Backend(format!(
            "{description} state transition is incomplete (current: {current:?}, pending: {pending:?}, expected: {expected:?})"
        )));
    }
    Ok(())
}

pub(crate) fn video_start_is_settled(
    current: PipelineLifecycleState,
    pending: PipelineLifecycleState,
    description: &str,
) -> Result<bool> {
    match (current, pending) {
        (PipelineLifecycleState::Playing, PipelineLifecycleState::VoidPending) => Ok(true),
        (PipelineLifecycleState::Paused, PipelineLifecycleState::Playing) => Ok(false),
        _ => Err(Error::Backend(format!(
            "{description} entered an invalid startup state (current: {current:?}, pending: {pending:?})"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_factual_settled_state_is_accepted() {
        require_settled_state(
            PipelineLifecycleState::Playing,
            PipelineLifecycleState::VoidPending,
            PipelineLifecycleState::Playing,
            "test",
        )
        .expect("settled playing");
        assert!(
            require_settled_state(
                PipelineLifecycleState::Paused,
                PipelineLifecycleState::Playing,
                PipelineLifecycleState::Playing,
                "test",
            )
            .is_err()
        );
        assert!(
            require_settled_state(
                PipelineLifecycleState::Playing,
                PipelineLifecycleState::Null,
                PipelineLifecycleState::Playing,
                "test",
            )
            .is_err()
        );
    }

    #[test]
    fn live_video_may_await_its_first_frame_but_nothing_else() {
        assert!(
            video_start_is_settled(
                PipelineLifecycleState::Playing,
                PipelineLifecycleState::VoidPending,
                "video",
            )
            .expect("settled")
        );
        assert!(
            !video_start_is_settled(
                PipelineLifecycleState::Paused,
                PipelineLifecycleState::Playing,
                "video",
            )
            .expect("awaiting first frame")
        );
        assert!(
            video_start_is_settled(
                PipelineLifecycleState::Ready,
                PipelineLifecycleState::Playing,
                "video",
            )
            .is_err()
        );
    }
}
