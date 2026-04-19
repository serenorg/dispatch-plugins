use anyhow::Result;

mod protocol;

fn main() -> Result<()> {
    channel_email_core::run::<protocol::Preset>()
}
