use anyhow::Result;

mod protocol;

fn main() -> Result<()> {
    Ok(channel_email_core::run::<protocol::Preset>()?)
}
