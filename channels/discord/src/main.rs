use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

mod discord_api;
mod protocol;

use discord_api::{DiscordClient, DiscordUpload};
use protocol::{
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelConfig, ChannelPolicy, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressCallbackReply, IngressMode, IngressPayload,
    IngressState, OutboundMessage, PluginRequest, PluginRequestEnvelope, PluginResponse,
    StatusAcceptance, StatusFrame, StatusKind, capabilities, parse_jsonrpc_request, plugin_error,
    response_to_jsonrpc,
};

const META_REASON: &str = "reason";
const META_PLATFORM: &str = "platform";
const META_BOT_TOKEN_ENV: &str = "bot_token_env";
const META_APPLICATION_ID: &str = "application_id";
const META_DEFAULT_CHANNEL_ID: &str = "default_channel_id";
const META_ALLOWED_GUILD_COUNT: &str = "allowed_guild_count";
const META_INGRESS_MODE: &str = "ingress_mode";
const META_INTERACTION_ENDPOINT: &str = "interaction_endpoint";
const META_INTERACTION_PUBLIC_KEY_ENV: &str = "interaction_public_key_env";
const META_VERIFICATION_KEY_ENV: &str = "verification_key_env";
const META_HOST_ACTION: &str = "host_action";
const META_CHANNEL_ID: &str = "channel_id";
const META_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const META_THREAD_ID: &str = "thread_id";
const META_TRANSPORT: &str = "transport";
const META_ENDPOINT_ID: &str = "endpoint_id";
const META_PATH: &str = "path";
const META_INTERACTION_TYPE: &str = "interaction_type";
const META_GUILD_ID: &str = "guild_id";
const META_COMMAND_NAME: &str = "command_name";
const META_COMMAND_KIND: &str = "command_kind";
const META_CUSTOM_ID: &str = "custom_id";
const META_COMPONENT_TYPE: &str = "component_type";
const META_SOURCE_MESSAGE_ID: &str = "source_message_id";
const META_ATTACHMENT_COUNT: &str = "attachment_count";
const META_LOCALE: &str = "locale";
const META_GUILD_LOCALE: &str = "guild_locale";
const META_ACTOR_KIND: &str = "actor_kind";
const META_NICK: &str = "nick";
const META_RESOLVED_DESTINATION: &str = "resolved_destination";
const META_STATUS_KIND: &str = "status_kind";
const META_REASON_CODE: &str = "reason_code";

const PLATFORM_DISCORD: &str = "discord";
const ROUTE_CONVERSATION_ID: &str = "conversation_id";
const ROUTE_THREAD_ID: &str = "thread_id";
const ROUTE_REPLY_TO_MESSAGE_ID: &str = "reply_to_message_id";
const TRANSPORT_INTERACTION_WEBHOOK: &str = "interaction_webhook";
const DISCORD_STATUS_TEXT: &str = "discord_status_text";

const HEADER_X_SIGNATURE_ED25519: &str = "X-Signature-Ed25519";
const HEADER_X_SIGNATURE_TIMESTAMP: &str = "X-Signature-Timestamp";

const INTERACTION_TYPE_PING: u8 = 1;
const INTERACTION_TYPE_APPLICATION_COMMAND: u8 = 2;
const INTERACTION_TYPE_MESSAGE_COMPONENT: u8 = 3;
const INTERACTION_TYPE_APPLICATION_COMMAND_AUTOCOMPLETE: u8 = 4;
const INTERACTION_TYPE_MODAL_SUBMIT: u8 = 5;

const COMMAND_KIND_CHAT_INPUT: u8 = 1;
const COMMAND_KIND_USER: u8 = 2;
const COMMAND_KIND_MESSAGE: u8 = 3;

const COMPONENT_KIND_BUTTON: u8 = 2;
const COMPONENT_KIND_STRING_SELECT: u8 = 3;
const COMPONENT_KIND_TEXT_INPUT: u8 = 4;
const COMPONENT_KIND_USER_SELECT: u8 = 5;
const COMPONENT_KIND_ROLE_SELECT: u8 = 6;
const COMPONENT_KIND_MENTIONABLE_SELECT: u8 = 7;
const COMPONENT_KIND_CHANNEL_SELECT: u8 = 8;

fn main() -> Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let (request_id, envelope) = parse_jsonrpc_request(&line)
            .map_err(|error| anyhow!("failed to parse channel request: {error}"))?;

        let response = match handle_request(&envelope) {
            Ok(response) => response,
            Err(error) => plugin_error("internal_error", error.to_string()),
        };

        let json = response_to_jsonrpc(&request_id, &response).map_err(|error| anyhow!(error))?;
        writeln!(stdout, "{json}")?;
        stdout.flush()?;
    }

    Ok(())
}

fn handle_request(envelope: &PluginRequestEnvelope) -> Result<PluginResponse> {
    if envelope.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return Ok(plugin_error(
            "unsupported_protocol_version",
            format!(
                "expected protocol_version {}, got {}",
                CHANNEL_PLUGIN_PROTOCOL_VERSION, envelope.protocol_version
            ),
        ));
    }

    match &envelope.request {
        PluginRequest::Capabilities => Ok(PluginResponse::Capabilities {
            capabilities: capabilities(),
        }),
        PluginRequest::Configure { config } => Ok(PluginResponse::Configured {
            configuration: Box::new(configure(config)?),
        }),
        PluginRequest::Health { config } => Ok(PluginResponse::Health {
            health: health(config)?,
        }),
        PluginRequest::StartIngress { config, .. } => Ok(PluginResponse::IngressStarted {
            state: start_ingress(config)?,
        }),
        PluginRequest::StopIngress { config, state } => Ok(PluginResponse::IngressStopped {
            state: stop_ingress(config, state.clone())?,
        }),
        PluginRequest::Deliver { config, message } => Ok(PluginResponse::Delivered {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::Push { config, message } => Ok(PluginResponse::Pushed {
            delivery: deliver(config, message)?,
        }),
        PluginRequest::IngressEvent {
            config, payload, ..
        } => handle_ingress_event(config, payload),
        PluginRequest::Status { config, update } => Ok(PluginResponse::StatusAccepted {
            status: send_status(config, update)?,
        }),
        PluginRequest::Shutdown => Ok(PluginResponse::Ok),
    }
}

fn configure(config: &ChannelConfig) -> Result<ConfiguredChannel> {
    let bot_token_env = bot_token_env(config);
    read_required_env(bot_token_env)?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_BOT_TOKEN_ENV.to_string(), bot_token_env.to_string());
    if let Some(application_id) = &config.application_id {
        metadata.insert(META_APPLICATION_ID.to_string(), application_id.clone());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        metadata.insert(
            META_DEFAULT_CHANNEL_ID.to_string(),
            default_channel_id.clone(),
        );
    }
    if !config.allowed_guild_ids.is_empty() {
        metadata.insert(
            META_ALLOWED_GUILD_COUNT.to_string(),
            config.allowed_guild_ids.len().to_string(),
        );
    }
    if let Some(endpoint) = resolved_endpoint(config) {
        let public_key_env = interaction_public_key_env(config);
        read_required_env(public_key_env)?;
        metadata.insert(
            META_INGRESS_MODE.to_string(),
            ingress_mode_name(IngressMode::InteractionWebhook),
        );
        metadata.insert(META_INTERACTION_ENDPOINT.to_string(), endpoint);
        metadata.insert(
            META_INTERACTION_PUBLIC_KEY_ENV.to_string(),
            public_key_env.to_string(),
        );
    }

    Ok(ConfiguredChannel {
        metadata,
        policy: Some(channel_policy(config)),
        runtime: None,
    })
}

fn channel_policy(config: &ChannelConfig) -> ChannelPolicy {
    ChannelPolicy {
        owner_id: config.owner_id.clone(),
        allowed_sender_ids: config.allowed_sender_ids.clone(),
        allowed_conversation_ids: config.allowed_guild_ids.clone(),
        dm_policy: config.dm_policy.clone(),
        require_signature_validation: Some(true),
        allow_group_messages: None,
        max_attachment_bytes: None,
        metadata: BTreeMap::new(),
    }
}

fn health(config: &ChannelConfig) -> Result<HealthReport> {
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let identity = client.identity()?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    if let Some(default_channel_id) = &config.default_channel_id {
        metadata.insert(
            META_DEFAULT_CHANNEL_ID.to_string(),
            default_channel_id.clone(),
        );
    }

    Ok(HealthReport {
        ok: true,
        status: "ok".to_string(),
        account_id: Some(identity.id),
        display_name: Some(identity.global_name.unwrap_or(identity.username)),
        metadata,
    })
}

fn start_ingress(config: &ChannelConfig) -> Result<IngressState> {
    read_required_env(bot_token_env(config))?;
    read_required_env(interaction_public_key_env(config))?;

    let endpoint = resolved_endpoint(config).ok_or_else(|| {
        anyhow!(
            "discord ingress requires webhook_public_url because this plugin uses interaction webhooks instead of a long-lived gateway connection"
        )
    })?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    metadata.insert(
        META_VERIFICATION_KEY_ENV.to_string(),
        interaction_public_key_env(config).to_string(),
    );
    metadata.insert(
        META_HOST_ACTION.to_string(),
        "route Discord interaction POSTs to the reported endpoint and verify signatures with the configured public key".to_string(),
    );

    Ok(IngressState {
        mode: IngressMode::InteractionWebhook,
        status: "configured".to_string(),
        endpoint: Some(endpoint),
        metadata,
    })
}

fn stop_ingress(config: &ChannelConfig, state: Option<IngressState>) -> Result<IngressState> {
    read_required_env(bot_token_env(config))?;

    let mut stopped = state.unwrap_or(IngressState {
        mode: IngressMode::InteractionWebhook,
        status: "configured".to_string(),
        endpoint: resolved_endpoint(config),
        metadata: BTreeMap::new(),
    });
    stopped.status = "stopped".to_string();
    stopped
        .metadata
        .insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    Ok(stopped)
}

fn handle_ingress_event(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<PluginResponse> {
    if !payload.method.eq_ignore_ascii_case("POST") {
        return Ok(ingress_rejection(
            405,
            "discord interactions expect POST requests",
        ));
    }

    if let Some(reply) = validate_ingress_signature(config, payload)? {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(reply),
            state: None,
            poll_after_ms: None,
        });
    }

    let interaction: DiscordInteraction = match serde_json::from_str(&payload.body) {
        Ok(interaction) => interaction,
        Err(_) => {
            return Ok(ingress_rejection(
                400,
                "invalid Discord interaction payload",
            ));
        }
    };

    if interaction.interaction_type == INTERACTION_TYPE_PING {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(discord_json_reply(json!({ "type": INTERACTION_TYPE_PING }))),
            state: None,
            poll_after_ms: None,
        });
    }

    if !guild_is_allowed(config, interaction.guild_id.as_deref()) {
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(discord_ephemeral_message(
                "This Discord server is not allowed for this plugin.",
            )),
            state: None,
            poll_after_ms: None,
        });
    }

    let Some(event) = build_inbound_event(config, &interaction, payload)? else {
        let message =
            if interaction.interaction_type == INTERACTION_TYPE_APPLICATION_COMMAND_AUTOCOMPLETE {
                "Discord autocomplete interactions are not implemented by this plugin."
            } else {
                "This Discord interaction type is not implemented by this plugin."
            };
        return Ok(PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: Some(discord_ephemeral_message(message)),
            state: None,
            poll_after_ms: None,
        });
    };

    Ok(PluginResponse::IngressEventsReceived {
        events: vec![event],
        callback_reply: Some(discord_ephemeral_message(
            "Dispatch is processing your request.",
        )),
        state: None,
        poll_after_ms: None,
    })
}

fn send_status(config: &ChannelConfig, update: &StatusFrame) -> Result<StatusAcceptance> {
    let content = render_status_message(update);
    if content.trim().is_empty() {
        return Ok(rejected_status(
            "missing_message",
            "discord status frames require a message or discord_status_text override",
        ));
    }

    let message = OutboundMessage {
        content,
        attachments: Vec::new(),
        channel_id: update
            .conversation_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_CONVERSATION_ID).cloned()),
        thread_id: update
            .thread_id
            .clone()
            .or_else(|| update.metadata.get(ROUTE_THREAD_ID).cloned()),
        reply_to_message_id: update.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned(),
        metadata: BTreeMap::new(),
    };

    let delivery = match deliver(config, &message) {
        Ok(delivery) => delivery,
        Err(error) => return Ok(rejected_status("delivery_failed", error.to_string())),
    };

    let mut metadata = delivery.metadata.clone();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    metadata.insert(META_STATUS_KIND.to_string(), status_kind_name(&update.kind));
    Ok(StatusAcceptance {
        accepted: true,
        metadata,
    })
}

fn deliver(config: &ChannelConfig, message: &OutboundMessage) -> Result<DeliveryReceipt> {
    let destination = resolve_destination(config, message)?;
    let reply_to_message_id = resolve_reply_to_message_id(message);
    let upload = discord_upload(message)?;
    let client = DiscordClient::from_env(bot_token_env(config))?;
    let posted = client.send_message(
        &destination,
        &message.content,
        reply_to_message_id.as_deref(),
        upload.as_ref(),
    )?;

    let mut metadata = BTreeMap::new();
    metadata.insert(META_PLATFORM.to_string(), PLATFORM_DISCORD.to_string());
    metadata.insert(META_CHANNEL_ID.to_string(), posted.channel_id.clone());
    metadata.insert(META_RESOLVED_DESTINATION.to_string(), destination.clone());
    if upload.is_some() {
        metadata.insert(META_ATTACHMENT_COUNT.to_string(), "1".to_string());
    }
    if let Some(reply_to_message_id) = &reply_to_message_id {
        metadata.insert(
            META_REPLY_TO_MESSAGE_ID.to_string(),
            reply_to_message_id.clone(),
        );
    }
    if let Some(thread_id) = resolve_thread_id(message) {
        metadata.insert(META_THREAD_ID.to_string(), thread_id.clone());
    }

    Ok(DeliveryReceipt {
        message_id: posted.id,
        conversation_id: posted.channel_id,
        metadata,
    })
}

fn discord_upload(message: &OutboundMessage) -> Result<Option<DiscordUpload>> {
    match message.attachments.as_slice() {
        [] => Ok(None),
        [attachment] => {
            if attachment.url.is_some() {
                bail!(
                    "discord outbound attachments require data_base64; url attachments are not supported"
                );
            }
            if attachment.storage_key.is_some() {
                bail!(
                    "discord outbound attachments require data_base64; storage_key attachments are not supported"
                );
            }
            let Some(data_base64) = attachment.data_base64.as_deref() else {
                bail!("discord outbound attachments require data_base64");
            };
            let data = BASE64_STANDARD.decode(data_base64).with_context(|| {
                format!(
                    "invalid base64 attachment payload for `{}`",
                    attachment.name
                )
            })?;
            Ok(Some(DiscordUpload {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                data,
            }))
        }
        _ => bail!("discord delivery supports at most one attachment"),
    }
}

fn build_inbound_event(
    config: &ChannelConfig,
    interaction: &DiscordInteraction,
    payload: &IngressPayload,
) -> Result<Option<InboundEventEnvelope>> {
    let event_type = interaction_type_name(interaction.interaction_type);
    let Some(event_type) = event_type else {
        return Ok(None);
    };

    let Some(actor) = inbound_actor(interaction) else {
        return Ok(None);
    };
    if !sender_is_allowed(config, &actor.id, interaction.guild_id.as_deref()) {
        return Ok(None);
    }
    let Some(conversation_id) = interaction.channel_id.clone() else {
        return Ok(None);
    };

    let received_at = received_at(payload.received_at.as_deref());
    let (content, mut message_metadata) = interaction_message(interaction);
    let attachments = extract_source_attachments(interaction, &mut message_metadata);
    let mut event_metadata = BTreeMap::new();
    event_metadata.insert(
        META_TRANSPORT.to_string(),
        TRANSPORT_INTERACTION_WEBHOOK.to_string(),
    );
    event_metadata.insert(META_INTERACTION_TYPE.to_string(), event_type.to_string());
    event_metadata.insert(
        META_APPLICATION_ID.to_string(),
        interaction.application_id.clone(),
    );
    if let Some(endpoint_id) = &payload.endpoint_id {
        event_metadata.insert(META_ENDPOINT_ID.to_string(), endpoint_id.clone());
    }
    if !payload.path.is_empty() {
        event_metadata.insert(META_PATH.to_string(), payload.path.clone());
    }
    if let Some(guild_id) = &interaction.guild_id {
        event_metadata.insert(META_GUILD_ID.to_string(), guild_id.clone());
    }
    if let Some(locale) = &interaction.locale {
        event_metadata.insert(META_LOCALE.to_string(), locale.clone());
    }
    if let Some(guild_locale) = &interaction.guild_locale {
        event_metadata.insert(META_GUILD_LOCALE.to_string(), guild_locale.clone());
    }

    let parent_message_id = interaction
        .message
        .as_ref()
        .map(|message| message.id.clone());
    if let Some(source_message_id) = &parent_message_id {
        message_metadata.insert(
            META_SOURCE_MESSAGE_ID.to_string(),
            source_message_id.clone(),
        );
    }

    Ok(Some(InboundEventEnvelope {
        event_id: interaction.id.clone(),
        platform: PLATFORM_DISCORD.to_string(),
        event_type: event_type.to_string(),
        received_at,
        conversation: InboundConversationRef {
            id: conversation_id,
            kind: conversation_kind(interaction),
            thread_id: None,
            parent_message_id: parent_message_id.clone(),
        },
        actor,
        message: InboundMessage {
            id: source_message_id_or_interaction_id(interaction),
            content,
            content_type: "text/plain".to_string(),
            reply_to_message_id: parent_message_id,
            attachments,
            metadata: message_metadata,
        },
        account_id: Some(interaction.application_id.clone()),
        metadata: event_metadata,
    }))
}

fn extract_source_attachments(
    interaction: &DiscordInteraction,
    message_metadata: &mut BTreeMap<String, String>,
) -> Vec<InboundAttachment> {
    let Some(source_message) = interaction.message.as_ref() else {
        return Vec::new();
    };

    let mut attachments = Vec::new();
    for attachment in &source_message.attachments {
        attachments.push(InboundAttachment {
            id: Some(attachment.id.clone()),
            kind: attachment_kind(attachment.content_type.as_deref()),
            url: Some(attachment.url.clone()),
            mime_type: attachment.content_type.clone(),
            size_bytes: attachment.size,
            name: Some(attachment.filename.clone()),
            storage_key: None,
            extracted_text: attachment.description.clone(),
            extras: BTreeMap::new(),
        });
    }

    if !attachments.is_empty() {
        message_metadata.insert(
            META_ATTACHMENT_COUNT.to_string(),
            attachments.len().to_string(),
        );
    }

    attachments
}

fn attachment_kind(content_type: Option<&str>) -> String {
    match content_type.and_then(|value| value.split('/').next()) {
        Some("image") => "image".to_string(),
        Some("audio") => "audio".to_string(),
        Some("video") => "video".to_string(),
        _ => "file".to_string(),
    }
}

fn interaction_message(interaction: &DiscordInteraction) -> (String, BTreeMap<String, String>) {
    let mut metadata = BTreeMap::new();
    let Some(data) = interaction.data.as_ref() else {
        return (String::new(), metadata);
    };

    match interaction.interaction_type {
        INTERACTION_TYPE_APPLICATION_COMMAND => {
            if let Some(name) = &data.name {
                metadata.insert(META_COMMAND_NAME.to_string(), name.clone());
            }
            if let Some(command_type) = data.command_type {
                metadata.insert(
                    META_COMMAND_KIND.to_string(),
                    command_kind_name(command_type).to_string(),
                );
            }
            let content = render_command_content(data);
            (content, metadata)
        }
        INTERACTION_TYPE_MESSAGE_COMPONENT => {
            if let Some(custom_id) = &data.custom_id {
                metadata.insert(META_CUSTOM_ID.to_string(), custom_id.clone());
            }
            if let Some(component_type) = data.component_type {
                metadata.insert(
                    META_COMPONENT_TYPE.to_string(),
                    component_type_name(component_type).to_string(),
                );
            }
            let content = render_component_content(data);
            (content, metadata)
        }
        INTERACTION_TYPE_MODAL_SUBMIT => {
            if let Some(custom_id) = &data.custom_id {
                metadata.insert(META_CUSTOM_ID.to_string(), custom_id.clone());
            }
            let content = render_modal_content(data);
            (content, metadata)
        }
        _ => (String::new(), metadata),
    }
}

fn render_command_content(data: &DiscordInteractionData) -> String {
    let Some(name) = data.name.as_deref() else {
        return "/command".to_string();
    };
    let mut rendered = format!("/{name}");
    let options = render_command_options(&data.options);
    if !options.is_empty() {
        rendered.push(' ');
        rendered.push_str(&options.join(" "));
    }
    rendered
}

fn render_command_options(options: &[DiscordCommandOption]) -> Vec<String> {
    let mut rendered = Vec::new();
    for option in options {
        if option.options.is_empty() {
            if let Some(value) = &option.value {
                rendered.push(format!("{}={}", option.name, render_json_scalar(value)));
            } else {
                rendered.push(option.name.clone());
            }
            continue;
        }

        rendered.push(option.name.clone());
        rendered.extend(render_command_options(&option.options));
    }
    rendered
}

fn render_component_content(data: &DiscordInteractionData) -> String {
    let custom_id = data.custom_id.as_deref().unwrap_or("component");
    if !data.values.is_empty() {
        return format!("component {custom_id}: {}", data.values.join(", "));
    }
    format!("component {custom_id}")
}

fn render_modal_content(data: &DiscordInteractionData) -> String {
    let custom_id = data.custom_id.as_deref().unwrap_or("modal");
    let values = collect_modal_values(&data.components);
    if values.is_empty() {
        return format!("modal {custom_id}");
    }
    format!("modal {custom_id}: {}", values.join(", "))
}

fn collect_modal_values(rows: &[DiscordComponentRow]) -> Vec<String> {
    let mut values = Vec::new();
    for row in rows {
        values.extend(collect_component_values(&row.components));
    }
    values
}

fn collect_component_values(components: &[DiscordComponentValue]) -> Vec<String> {
    let mut values = Vec::new();
    for component in components {
        if let Some(value) = &component.value {
            if let Some(custom_id) = &component.custom_id {
                values.push(format!("{custom_id}={value}"));
            } else {
                values.push(value.clone());
            }
        }
        if !component.values.is_empty() {
            if let Some(custom_id) = &component.custom_id {
                values.push(format!("{custom_id}={}", component.values.join("|")));
            } else {
                values.extend(component.values.clone());
            }
        }
        if !component.components.is_empty() {
            values.extend(collect_component_values(&component.components));
        }
    }
    values
}

fn render_json_scalar(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn inbound_actor(interaction: &DiscordInteraction) -> Option<InboundActor> {
    if let Some(member) = &interaction.member
        && let Some(user) = &member.user
    {
        let mut metadata = BTreeMap::new();
        metadata.insert(META_ACTOR_KIND.to_string(), "member".to_string());
        if let Some(nick) = &member.nick {
            metadata.insert(META_NICK.to_string(), nick.clone());
        }
        return Some(InboundActor {
            id: user.id.clone(),
            display_name: member
                .nick
                .clone()
                .or_else(|| user.global_name.clone())
                .or_else(|| Some(user.username.clone())),
            username: Some(user.username.clone()),
            is_bot: user.bot.unwrap_or(false),
            metadata,
        });
    }

    interaction.user.as_ref().map(|user| InboundActor {
        id: user.id.clone(),
        display_name: user
            .global_name
            .clone()
            .or_else(|| Some(user.username.clone())),
        username: Some(user.username.clone()),
        is_bot: user.bot.unwrap_or(false),
        metadata: BTreeMap::from([(META_ACTOR_KIND.to_string(), "user".to_string())]),
    })
}

fn validate_ingress_signature(
    config: &ChannelConfig,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    if payload.trust_verified {
        return Ok(None);
    }

    let public_key = read_required_env(interaction_public_key_env(config))?;
    validate_discord_signature(&public_key, payload)
}

fn validate_discord_signature(
    public_key_hex: &str,
    payload: &IngressPayload,
) -> Result<Option<IngressCallbackReply>> {
    let Some(signature_hex) = header_value(&payload.headers, HEADER_X_SIGNATURE_ED25519) else {
        return Ok(Some(callback_reply(
            401,
            "discord request signature header missing",
        )));
    };
    let Some(timestamp) = header_value(&payload.headers, HEADER_X_SIGNATURE_TIMESTAMP) else {
        return Ok(Some(callback_reply(
            401,
            "discord request timestamp header missing",
        )));
    };

    let public_key_bytes =
        hex::decode(public_key_hex).context("invalid Discord interaction public key")?;
    let public_key_bytes: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| anyhow!("Discord interaction public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .context("invalid Discord interaction public key bytes")?;

    let signature_bytes =
        hex::decode(signature_hex).context("invalid X-Signature-Ed25519 header")?;
    let signature = Signature::try_from(signature_bytes.as_slice())
        .map_err(|_| anyhow!("invalid Discord interaction signature bytes"))?;

    let signed_message = format!("{timestamp}{}", payload.body);
    if verifying_key
        .verify(signed_message.as_bytes(), &signature)
        .is_err()
    {
        return Ok(Some(callback_reply(401, "invalid request signature")));
    }

    Ok(None)
}

fn resolve_destination(config: &ChannelConfig, message: &OutboundMessage) -> Result<String> {
    if let Some(thread_id) = resolve_thread_id(message) {
        return Ok(thread_id.clone());
    }
    if let Some(channel_id) = &message.channel_id {
        return Ok(channel_id.clone());
    }
    if let Some(channel_id) = message.metadata.get(ROUTE_CONVERSATION_ID) {
        return Ok(channel_id.clone());
    }
    if let Some(default_channel_id) = &config.default_channel_id {
        return Ok(default_channel_id.clone());
    }
    Err(anyhow!(
        "discord delivery requires message.thread_id, message.channel_id, or config.default_channel_id"
    ))
}

fn resolve_thread_id(message: &OutboundMessage) -> Option<&String> {
    message
        .thread_id
        .as_ref()
        .or_else(|| message.metadata.get(ROUTE_THREAD_ID))
}

fn resolve_reply_to_message_id(message: &OutboundMessage) -> Option<String> {
    message
        .reply_to_message_id
        .clone()
        .or_else(|| message.metadata.get(ROUTE_REPLY_TO_MESSAGE_ID).cloned())
}

fn resolved_endpoint(config: &ChannelConfig) -> Option<String> {
    let base = config.webhook_public_url.as_deref()?.trim_end_matches('/');
    let path = config
        .webhook_path
        .as_deref()
        .unwrap_or("/discord/interactions")
        .trim_start_matches('/');
    Some(format!("{base}/{path}"))
}

fn bot_token_env(config: &ChannelConfig) -> &str {
    config
        .bot_token_env
        .as_deref()
        .unwrap_or("DISCORD_BOT_TOKEN")
}

fn interaction_public_key_env(config: &ChannelConfig) -> &str {
    config
        .interaction_public_key_env
        .as_deref()
        .unwrap_or("DISCORD_INTERACTION_PUBLIC_KEY")
}

fn conversation_kind(interaction: &DiscordInteraction) -> String {
    if interaction.guild_id.is_some() {
        "channel".to_string()
    } else {
        "direct_message".to_string()
    }
}

fn guild_is_allowed(config: &ChannelConfig, guild_id: Option<&str>) -> bool {
    config.allowed_guild_ids.is_empty()
        || guild_id
            .map(|guild_id| {
                config
                    .allowed_guild_ids
                    .iter()
                    .any(|allowed| allowed == guild_id)
            })
            .unwrap_or(false)
}

fn sender_is_allowed(config: &ChannelConfig, sender_id: &str, guild_id: Option<&str>) -> bool {
    if let Some(owner_id) = config.owner_id.as_deref()
        && owner_id != sender_id
    {
        return false;
    }

    if guild_id.is_some() {
        return true;
    }

    match config.dm_policy.as_deref() {
        Some("deny") => false,
        Some("pairing") => id_matches_allowlist(&config.allowed_sender_ids, sender_id),
        Some("open") | None => true,
        Some(_) => true,
    }
}

fn id_matches_allowlist(allowlist: &[String], value: &str) -> bool {
    allowlist
        .iter()
        .any(|allowed| allowed == "*" || allowed == value)
}

fn source_message_id_or_interaction_id(interaction: &DiscordInteraction) -> String {
    interaction
        .message
        .as_ref()
        .map(|message| message.id.clone())
        .unwrap_or_else(|| interaction.id.clone())
}

fn received_at(host_received_at: Option<&str>) -> String {
    host_received_at
        .map(str::to_owned)
        .unwrap_or_else(|| Timestamp::now().to_string())
}

fn ingress_rejection(status: u16, message: &str) -> PluginResponse {
    PluginResponse::IngressEventsReceived {
        events: Vec::new(),
        callback_reply: Some(callback_reply(status, message)),
        state: None,
        poll_after_ms: None,
    }
}

fn callback_reply(status: u16, message: &str) -> IngressCallbackReply {
    IngressCallbackReply {
        status,
        content_type: Some("text/plain; charset=utf-8".to_string()),
        body: message.to_string(),
    }
}

fn discord_json_reply(body: Value) -> IngressCallbackReply {
    IngressCallbackReply {
        status: 200,
        content_type: Some("application/json".to_string()),
        body: body.to_string(),
    }
}

fn discord_ephemeral_message(message: &str) -> IngressCallbackReply {
    discord_json_reply(json!({
        "type": 4,
        "data": {
            "content": message,
            "flags": 64
        }
    }))
}

fn render_status_message(update: &StatusFrame) -> String {
    if let Some(status_text) = update.metadata.get(DISCORD_STATUS_TEXT) {
        return status_text.clone();
    }

    let prefix = match update.kind {
        StatusKind::Processing => "Processing",
        StatusKind::Completed => "Completed",
        StatusKind::Cancelled => "Cancelled",
        StatusKind::OperationStarted => "Started",
        StatusKind::OperationFinished => "Finished",
        StatusKind::ApprovalNeeded => "Approval needed",
        StatusKind::Info => "Info",
        StatusKind::Delivering => "Delivering",
        StatusKind::AuthRequired => "Authentication required",
        _ => "Status",
    };

    if update.message.trim().is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {}", update.message)
    }
}

fn interaction_type_name(interaction_type: u8) -> Option<&'static str> {
    match interaction_type {
        INTERACTION_TYPE_APPLICATION_COMMAND => Some("application_command"),
        INTERACTION_TYPE_MESSAGE_COMPONENT => Some("message_component"),
        INTERACTION_TYPE_MODAL_SUBMIT => Some("modal_submit"),
        _ => None,
    }
}

fn command_kind_name(command_type: u8) -> &'static str {
    match command_type {
        COMMAND_KIND_CHAT_INPUT => "chat_input",
        COMMAND_KIND_USER => "user",
        COMMAND_KIND_MESSAGE => "message",
        _ => "unknown",
    }
}

fn component_type_name(component_type: u8) -> &'static str {
    match component_type {
        COMPONENT_KIND_BUTTON => "button",
        COMPONENT_KIND_STRING_SELECT => "string_select",
        COMPONENT_KIND_TEXT_INPUT => "text_input",
        COMPONENT_KIND_USER_SELECT => "user_select",
        COMPONENT_KIND_ROLE_SELECT => "role_select",
        COMPONENT_KIND_MENTIONABLE_SELECT => "mentionable_select",
        COMPONENT_KIND_CHANNEL_SELECT => "channel_select",
        _ => "unknown",
    }
}

fn ingress_mode_name(mode: IngressMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "interaction_webhook".to_string())
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn rejected_status(reason_code: &str, reason: impl Into<String>) -> StatusAcceptance {
    StatusAcceptance {
        accepted: false,
        metadata: BTreeMap::from([
            (META_REASON_CODE.to_string(), reason_code.to_string()),
            (META_REASON.to_string(), reason.into()),
        ]),
    }
}

fn status_kind_name(kind: &StatusKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn read_required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required for the discord channel"))
}

#[derive(Debug, Deserialize)]
struct DiscordInteraction {
    id: String,
    application_id: String,
    #[serde(rename = "type")]
    interaction_type: u8,
    #[serde(default)]
    data: Option<DiscordInteractionData>,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    member: Option<DiscordInteractionMember>,
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    message: Option<DiscordSourceMessage>,
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    guild_locale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionData {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    command_type: Option<u8>,
    #[serde(default)]
    custom_id: Option<String>,
    #[serde(default)]
    component_type: Option<u8>,
    #[serde(default)]
    options: Vec<DiscordCommandOption>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    components: Vec<DiscordComponentRow>,
}

#[derive(Debug, Deserialize)]
struct DiscordCommandOption {
    name: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    options: Vec<DiscordCommandOption>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionMember {
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    nick: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    bot: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DiscordSourceMessage {
    id: String,
    #[serde(default)]
    attachments: Vec<DiscordSourceAttachment>,
}

#[derive(Debug, Deserialize)]
struct DiscordSourceAttachment {
    id: String,
    filename: String,
    url: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordComponentRow {
    #[serde(default)]
    components: Vec<DiscordComponentValue>,
}

#[derive(Debug, Deserialize)]
struct DiscordComponentValue {
    #[serde(default)]
    custom_id: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    components: Vec<DiscordComponentValue>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::OutboundAttachment;
    use ed25519_dalek::{Signer, SigningKey};

    fn base_payload(body: &str) -> IngressPayload {
        IngressPayload {
            endpoint_id: Some("discord:/discord/interactions".to_string()),
            method: "POST".to_string(),
            path: "/discord/interactions".to_string(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            raw_query: None,
            body: body.to_string(),
            trust_verified: true,
            received_at: Some("2026-04-11T21:00:00Z".to_string()),
        }
    }

    #[test]
    fn ping_returns_pong_callback() {
        let payload = base_payload(r#"{"id":"1","application_id":"app-1","type":1}"#);

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert!(events.is_empty());
                let reply = callback_reply.expect("pong reply");
                assert_eq!(reply.status, 200);
                assert_eq!(reply.content_type.as_deref(), Some("application/json"));
                assert_eq!(reply.body, r#"{"type":1}"#);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn command_interaction_maps_to_inbound_event() {
        let payload = base_payload(
            r#"{
                "id":"interaction-1",
                "application_id":"app-1",
                "type":2,
                "guild_id":"guild-1",
                "channel_id":"channel-1",
                "locale":"en-US",
                "guild_locale":"en-US",
                "member":{
                    "nick":"Dispatch User",
                    "user":{
                        "id":"user-1",
                        "username":"dispatch-user",
                        "global_name":"Dispatch User"
                    }
                },
                "data":{
                    "name":"ask",
                    "type":1,
                    "options":[
                        {
                            "name":"query",
                            "value":"hello world"
                        }
                    ]
                }
            }"#,
        );

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert_eq!(events.len(), 1);
                let reply = callback_reply.expect("interaction ack");
                assert_eq!(reply.status, 200);
                assert!(reply.body.contains("Dispatch is processing your request."));

                let event = &events[0];
                assert_eq!(event.event_id, "interaction-1");
                assert_eq!(event.platform, PLATFORM_DISCORD);
                assert_eq!(event.event_type, "application_command");
                assert_eq!(event.account_id.as_deref(), Some("app-1"));
                assert_eq!(event.conversation.id, "channel-1");
                assert_eq!(event.conversation.kind, "channel");
                assert_eq!(event.actor.id, "user-1");
                assert_eq!(event.actor.display_name.as_deref(), Some("Dispatch User"));
                assert_eq!(event.message.id, "interaction-1");
                assert_eq!(event.message.content, "/ask query=hello world");
                assert_eq!(
                    event.metadata.get(META_TRANSPORT).map(String::as_str),
                    Some(TRANSPORT_INTERACTION_WEBHOOK)
                );
                assert_eq!(
                    event.metadata.get(META_ENDPOINT_ID).map(String::as_str),
                    Some("discord:/discord/interactions")
                );
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_COMMAND_NAME)
                        .map(String::as_str),
                    Some("ask")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn component_interaction_preserves_source_message_attachments() {
        let payload = base_payload(
            r#"{
                "id":"interaction-2",
                "application_id":"app-1",
                "type":3,
                "guild_id":"guild-1",
                "channel_id":"channel-1",
                "member":{
                    "user":{
                        "id":"user-2",
                        "username":"dispatch-user"
                    }
                },
                "data":{
                    "custom_id":"approve",
                    "component_type":2
                },
                "message":{
                    "id":"message-99",
                    "attachments":[
                        {
                            "id":"attachment-1",
                            "filename":"design.png",
                            "url":"https://cdn.discordapp.com/attachments/design.png",
                            "content_type":"image/png",
                            "size":4096,
                            "description":"wireframe"
                        }
                    ]
                }
            }"#,
        );

        let response =
            handle_ingress_event(&ChannelConfig::default(), &payload).expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived {
                events,
                callback_reply,
                ..
            } => {
                assert_eq!(events.len(), 1);
                assert!(callback_reply.is_some());

                let event = &events[0];
                assert_eq!(event.event_type, "message_component");
                assert_eq!(event.message.id, "message-99");
                assert_eq!(
                    event.message.reply_to_message_id.as_deref(),
                    Some("message-99")
                );
                assert_eq!(event.message.attachments.len(), 1);

                let attachment = &event.message.attachments[0];
                assert_eq!(attachment.id.as_deref(), Some("attachment-1"));
                assert_eq!(attachment.kind, "image");
                assert_eq!(
                    attachment.url.as_deref(),
                    Some("https://cdn.discordapp.com/attachments/design.png")
                );
                assert_eq!(attachment.mime_type.as_deref(), Some("image/png"));
                assert_eq!(attachment.size_bytes, Some(4096));
                assert_eq!(attachment.name.as_deref(), Some("design.png"));
                assert_eq!(attachment.extracted_text.as_deref(), Some("wireframe"));
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_ATTACHMENT_COUNT)
                        .map(String::as_str),
                    Some("1")
                );
                assert_eq!(
                    event
                        .message
                        .metadata
                        .get(META_CUSTOM_ID)
                        .map(String::as_str),
                    Some("approve")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let mut payload = base_payload(r#"{"id":"1","application_id":"app-1","type":1}"#);
        payload.trust_verified = false;
        payload
            .headers
            .insert(HEADER_X_SIGNATURE_ED25519.to_string(), "00".repeat(64));
        payload.headers.insert(
            HEADER_X_SIGNATURE_TIMESTAMP.to_string(),
            "1712878800".to_string(),
        );

        let reply = validate_discord_signature("11".repeat(32).as_str(), &payload)
            .expect("validate signature")
            .expect("rejection reply");

        assert_eq!(reply.status, 401);
        assert_eq!(reply.body, "invalid request signature");
    }

    #[test]
    fn valid_signature_is_accepted() {
        let body = r#"{"id":"1","application_id":"app-1","type":1}"#;
        let timestamp = "1712878800";
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let verify_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(format!("{timestamp}{body}").as_bytes());

        let mut payload = base_payload(body);
        payload.trust_verified = false;
        payload.headers.insert(
            HEADER_X_SIGNATURE_ED25519.to_string(),
            hex::encode(signature.to_bytes()),
        );
        payload.headers.insert(
            HEADER_X_SIGNATURE_TIMESTAMP.to_string(),
            timestamp.to_string(),
        );

        let reply =
            validate_discord_signature(&verify_key_hex, &payload).expect("validate signature");

        assert!(reply.is_none());
    }

    #[test]
    fn direct_message_is_dropped_when_pairing_policy_has_no_allowlist_match() {
        let payload = base_payload(
            r#"{
                "id":"interaction-3",
                "application_id":"app-1",
                "type":2,
                "channel_id":"dm-channel-1",
                "member":{
                    "user":{
                        "id":"user-9",
                        "username":"dispatch-user"
                    }
                },
                "data":{
                    "name":"ask",
                    "type":1,
                    "options":[
                        {
                            "name":"query",
                            "value":"hello world"
                        }
                    ]
                }
            }"#,
        );

        let response = handle_ingress_event(
            &ChannelConfig {
                dm_policy: Some("pairing".to_string()),
                ..ChannelConfig::default()
            },
            &payload,
        )
        .expect("handle ingress");

        match response {
            PluginResponse::IngressEventsReceived { events, .. } => assert!(events.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn render_status_message_uses_override() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "ignored".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::from([(DISCORD_STATUS_TEXT.to_string(), "custom".to_string())]),
        };

        assert_eq!(render_status_message(&update), "custom");
    }

    #[test]
    fn send_status_rejects_missing_destination() {
        let update = StatusFrame {
            kind: StatusKind::Info,
            message: "hello".to_string(),
            conversation_id: None,
            thread_id: None,
            metadata: BTreeMap::new(),
        };

        let acceptance = send_status(&ChannelConfig::default(), &update).expect("send status");

        assert!(!acceptance.accepted);
        assert_eq!(
            acceptance
                .metadata
                .get(META_REASON_CODE)
                .map(String::as_str),
            Some("delivery_failed")
        );
    }

    #[test]
    fn discord_upload_decodes_inline_attachment_data() {
        let message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: None,
                storage_key: None,
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };

        let upload = discord_upload(&message)
            .expect("inline attachment")
            .expect("upload present");
        assert_eq!(upload.name, "report.txt");
        assert_eq!(upload.mime_type, "text/plain");
        assert_eq!(upload.data, b"hello");
    }

    #[test]
    fn discord_upload_rejects_url_and_storage_key_sources() {
        let url_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: Some("https://example.com/report.txt".to_string()),
                storage_key: None,
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            discord_upload(&url_message)
                .expect_err("url attachments rejected")
                .to_string()
                .contains("url attachments are not supported")
        );

        let staged_message = OutboundMessage {
            content: "hello".to_string(),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: None,
                storage_key: Some("cache://report.txt".to_string()),
            }],
            channel_id: Some("123".to_string()),
            thread_id: None,
            reply_to_message_id: None,
            metadata: BTreeMap::new(),
        };
        assert!(
            discord_upload(&staged_message)
                .expect_err("storage_key attachments rejected")
                .to_string()
                .contains("storage_key attachments are not supported")
        );
    }
}
