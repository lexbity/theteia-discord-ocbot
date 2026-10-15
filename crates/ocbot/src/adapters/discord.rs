//! The serenity adapter (spec §6.6): the only code that builds Discord
//! payloads, and therefore the single structural enforcement point for FR-3
//! (`allowed_mentions { parse: [], users: [OPERATOR] }`). Every payload goes
//! through [`message_builder`], which always sets it, and the shared `Http` is
//! seeded with the same restriction as a defense-in-depth default.
//!
//! Interactions are normalized into [`InteractionSource`] and routed through
//! the two-phase flow: the plan phase (`router::guarded_plan`) produces the
//! immediate ack (modal or deferral) which is sent before any handler work;
//! the execute pass then runs in a spawned task.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serenity::all::{
    ActionRowComponent, ButtonStyle, ChannelId, Client, Context, EventHandler, GatewayIntents,
    GuildId, Http, InputTextStyle, Interaction, Message, MessageId, ReactionType, Ready,
    ShardManager, UserId,
};
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateCommand,
    CreateInputText, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, CreateMessage, CreateModal, EditMessage,
};

use ocbot_core::{ButtonKind, Destination, OutgoingMessage, PostReceipt, Surface};

use crate::app::context::{AppContext, BroadcastOutcome, ChatPort, SendError};
use crate::app::handlers;
use crate::app::interaction::{
    AckPlan, InputStyle, InteractionSource, ModalSpec, SourceKind, TextInput,
};
use crate::app::router::{self, PlanOutcome};

/// A shared, structurally-locked `Http`: the default `allowed_mentions` is the
/// operator-only restriction, so even a future code path that forgets to set
/// it explicitly cannot mention `@everyone` (FR-3 belt).
pub fn shared_http(token: &str, operator: u64) -> Arc<Http> {
    let mut http = Http::new(token);
    http.default_allowed_mentions = Some(operator_allowed_mentions(operator));
    Arc::new(http)
}

/// The single allowed-mentions constructor (FR-3). Nothing else in the
/// codebase builds one. `parse` defaults to the empty set (nothing pings);
/// only the operator may be mentioned. Public so the structural guarantee is
/// asserted by `discord_payload.rs`.
pub fn operator_allowed_mentions(operator: u64) -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .users([UserId::new(operator)])
}

/// The only production Discord [`ChatPort`]: posts through serenity `Http`.
/// Ids are parsed at this boundary (SPEC-004 D-02): a `chat_ref`/`msg_id` that
/// is not a numeric snowflake is a programming error and fails loud.
///
/// Discord edits/reactions need the channel, but the port contract's
/// [`PostReceipt`] carries only the message id (surface-agnostic by design);
/// the adapter therefore remembers the channel of every message it posted in
/// a bounded map (quiet ops only — a miss degrades to a warn, never an error
/// for the caller).
#[derive(Clone)]
pub struct DiscordAdapter {
    http: Arc<Http>,
    operator: u64,
    /// The announce destination for `broadcast` (the ops channel; PH-1's
    /// single-surface stand-in for the PH-6 fan-out).
    announce: Option<u64>,
    /// Bounded `msg_id → channel` memory for receipt-addressed quiet ops.
    channels: Arc<Mutex<HashMap<u64, u64>>>,
}

/// The map bound (quiet-op memory, not correctness state).
const CHANNEL_MEMORY_CAP: usize = 4_096;

impl DiscordAdapter {
    /// Wrap a shared, structurally-locked http handle.
    pub fn new(http: Arc<Http>, operator: u64) -> Self {
        Self {
            http,
            operator,
            announce: None,
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set the announce destination used by `broadcast`.
    #[must_use]
    pub fn with_announce(mut self, channel: Option<u64>) -> Self {
        self.announce = channel;
        self
    }

    fn remember_channel(&self, message: u64, channel: u64) {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if channels.len() >= CHANNEL_MEMORY_CAP {
            channels.clear();
        }
        channels.insert(message, channel);
    }

    fn channel_of(&self, message: u64) -> Option<u64> {
        self.channels
            .lock()
            .ok()
            .and_then(|channels| channels.get(&message).copied())
    }
}

/// Parse a snowflake from an opaque core string, failing loud (D-02).
fn snowflake(text: &str, what: &str) -> Result<u64, SendError> {
    text.parse().map_err(|_| SendError {
        surface: Surface::Discord,
        message: format!("{what} {text:?} is not a numeric snowflake"),
    })
}

/// Core spec → `CreateMessage`. Always sets `allowed_mentions`. Public so the
/// structural guarantee is unit-testable (`discord_payload.rs`); only the
/// adapter calls it in production.
pub fn message_builder(spec: &OutgoingMessage, operator: u64) -> CreateMessage {
    let mut builder = CreateMessage::new()
        .content(spec.content.clone())
        .allowed_mentions(operator_allowed_mentions(operator));
    if let Some(attachment) = &spec.attachment {
        builder = builder.add_file(CreateAttachment::bytes(
            attachment.bytes.clone(),
            attachment.filename.clone(),
        ));
    }
    if !spec.components.is_empty() {
        // Discord allows at most five buttons per action row; a page of history
        // can carry reset buttons plus paging, so chunk into rows of five.
        let rows = spec
            .components
            .chunks(5)
            .map(|chunk| {
                let buttons = chunk
                    .iter()
                    .map(|component| {
                        let style = match component.kind {
                            ButtonKind::Primary => ButtonStyle::Primary,
                            ButtonKind::Secondary => ButtonStyle::Secondary,
                            ButtonKind::Danger => ButtonStyle::Danger,
                            ButtonKind::Success => ButtonStyle::Success,
                        };
                        CreateButton::new(component.custom_id.clone())
                            .label(component.label.clone())
                            .style(style)
                            .disabled(component.disabled)
                    })
                    .collect();
                CreateActionRow::Buttons(buttons)
            })
            .collect();
        builder = builder.components(rows);
    }
    builder
}

#[async_trait]
impl ChatPort for DiscordAdapter {
    async fn post(
        &self,
        dest: &Destination,
        spec: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        let channel = snowflake(&dest.chat_ref.0, "chat_ref")?;
        let message = ChannelId::new(channel)
            .send_message(&*self.http, message_builder(spec, self.operator))
            .await
            .map_err(|error| SendError {
                surface: Surface::Discord,
                message: error.to_string(),
            })?;
        self.remember_channel(message.id.get(), channel);
        Ok(PostReceipt {
            surface: Surface::Discord,
            msg_id: ocbot_core::MsgId(message.id.get().to_string()),
        })
    }

    async fn edit(&self, receipt: &PostReceipt, replacement: &OutgoingMessage) {
        let result = self.edit_inner(receipt, replacement).await;
        if let Err(error) = result {
            tracing::warn!(error = %error, "quiet edit failed");
        }
    }

    async fn react(&self, receipt: &PostReceipt, emoji: char) {
        let result = self.react_inner(receipt, emoji).await;
        if let Err(error) = result {
            tracing::warn!(error = %error, "status reaction failed");
        }
    }

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        let mut outcome = BroadcastOutcome::default();
        match self.announce {
            Some(channel) => match self.post(&Destination::discord(channel), message).await {
                Ok(receipt) => outcome.delivered.push((Surface::Discord, receipt)),
                Err(error) => outcome.failed.push((Surface::Discord, error)),
            },
            None => {
                outcome.failed.push((
                    Surface::Discord,
                    SendError {
                        surface: Surface::Discord,
                        message: "no announce destination configured".to_owned(),
                    },
                ));
            }
        }
        outcome
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![Surface::Discord]
    }
}

impl DiscordAdapter {
    async fn edit_inner(
        &self,
        receipt: &PostReceipt,
        replacement: &OutgoingMessage,
    ) -> Result<(), SendError> {
        let message = snowflake(&receipt.msg_id.0, "msg_id")?;
        let channel = self.channel_of(message).ok_or_else(|| SendError {
            surface: Surface::Discord,
            message: format!("no channel remembered for message {message}"),
        })?;
        let builder = EditMessage::new().content(replacement.content.clone());
        self.http
            .edit_message(
                ChannelId::new(channel),
                MessageId::new(message),
                &builder,
                Vec::new(),
            )
            .await
            .map(|_| ())
            .map_err(|error| SendError {
                surface: Surface::Discord,
                message: error.to_string(),
            })
    }

    async fn react_inner(&self, receipt: &PostReceipt, emoji: char) -> Result<(), SendError> {
        let message = snowflake(&receipt.msg_id.0, "msg_id")?;
        let channel = self.channel_of(message).ok_or_else(|| SendError {
            surface: Surface::Discord,
            message: format!("no channel remembered for message {message}"),
        })?;
        let reaction: ReactionType = emoji.into();
        self.http
            .create_reaction(ChannelId::new(channel), MessageId::new(message), &reaction)
            .await
            .map_err(|error| SendError {
                surface: Surface::Discord,
                message: error.to_string(),
            })
    }
}

/// The gateway event handler.
pub struct BotEvents {
    ctx: Arc<AppContext>,
    http: Arc<Http>,
}

impl BotEvents {
    /// Wrap the app context and the shared http.
    pub fn new(ctx: Arc<AppContext>, http: Arc<Http>) -> Self {
        Self { ctx, http }
    }

    /// Send the immediate ack called for by the plan.
    async fn ack(&self, ctx: &Context, interaction: &Interaction, plan: &AckPlan) {
        let result: Result<(), serenity::Error> = match (interaction, plan) {
            (Interaction::Command(command), AckPlan::Modal(spec)) => {
                command
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Modal(build_modal(spec)),
                    )
                    .await
            }
            (Interaction::Command(command), AckPlan::DeferPublic) => command.defer(&ctx.http).await,
            (Interaction::Command(command), AckPlan::DeferEphemeral) => {
                command.defer_ephemeral(&ctx.http).await
            }
            (Interaction::Component(component), AckPlan::Modal(spec)) => {
                component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Modal(build_modal(spec)),
                    )
                    .await
            }
            (Interaction::Component(component), AckPlan::DeferUpdate) => {
                component.defer(&ctx.http).await
            }
            (Interaction::Modal(modal), AckPlan::DeferPublic) => modal.defer(&ctx.http).await,
            (Interaction::Modal(modal), AckPlan::DeferEphemeral) => {
                modal.defer_ephemeral(&ctx.http).await
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            tracing::warn!(error = %error, "interaction ack failed");
        }
    }

    /// Ack first, then run the execute pass in a spawned task (never blocking
    /// the gateway on slow work).
    async fn act(
        &self,
        ctx: &Context,
        interaction: Interaction,
        source: InteractionSource,
        plan: AckPlan,
    ) {
        self.ack(ctx, &interaction, &plan).await;
        if matches!(
            plan,
            AckPlan::DeferPublic | AckPlan::DeferEphemeral | AckPlan::DeferUpdate
        ) {
            let app_ctx = self.ctx.clone();
            let http = self.http.clone();
            tokio::spawn(run_execute(app_ctx, http, interaction, source));
        }
    }
}

async fn run_execute(
    ctx: Arc<AppContext>,
    http: Arc<Http>,
    interaction: Interaction,
    source: InteractionSource,
) {
    let outcome = router::execute(&ctx, &source).await;
    match outcome {
        Ok(Some(text)) => followup_ephemeral(&http, &interaction, &text).await,
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(error = %error, "interaction execute failed");
            followup_ephemeral(&http, &interaction, &format!("error: {error}")).await;
        }
    }
}

/// Ephemeral followup on the originating interaction (only valid after the
/// deferred ack, within 15 minutes).
async fn followup_ephemeral(http: &Http, interaction: &Interaction, text: &str) {
    if text.is_empty() {
        return;
    }
    let builder = CreateInteractionResponseFollowup::new()
        .content(text)
        .ephemeral(true);
    let result: Result<Message, serenity::Error> = match interaction {
        Interaction::Command(command) => command.create_followup(http, builder).await,
        Interaction::Component(component) => component.create_followup(http, builder).await,
        Interaction::Modal(modal) => modal.create_followup(http, builder).await,
        _ => return,
    };
    if let Err(error) = result {
        tracing::warn!(error = %error, "followup failed");
    }
}

/// Immediate (non-deferred) response with optional ephemeral content — used
/// for the deny and failed-plan paths.
async fn respond_now(http: &Http, interaction: &Interaction, text: Option<&str>) {
    let mut data = CreateInteractionResponseMessage::new();
    if let Some(text) = text {
        data = data.content(text).ephemeral(true);
    }
    let response = CreateInteractionResponse::Message(data);
    let result: Result<(), serenity::Error> = match interaction {
        Interaction::Command(command) => command.create_response(http, response).await,
        Interaction::Component(component) => component.create_response(http, response).await,
        Interaction::Modal(modal) => modal.create_response(http, response).await,
        _ => return,
    };
    if let Err(error) = result {
        tracing::warn!(error = %error, "response ack failed");
    }
}

/// Build a `CreateModal` from our spec (≤5 inputs).
fn build_modal(spec: &ModalSpec) -> CreateModal {
    let rows = spec
        .inputs
        .iter()
        .map(|input| {
            let style = match input.style {
                InputStyle::Short => InputTextStyle::Short,
                InputStyle::Paragraph => InputTextStyle::Paragraph,
            };
            let mut builder = CreateInputText::new(style, input.label.clone(), input.id.clone());
            if let Some(value) = &input.value {
                builder = builder.value(value.clone());
            }
            builder = builder.required(input.required);
            CreateActionRow::InputText(builder)
        })
        .collect();
    CreateModal::new(spec.custom_id.clone(), spec.title.clone()).components(rows)
}

/// Normalize a Discord interaction into our vocabulary; `None` for kinds the
/// bot never routes (ping, autocomplete).
fn build_source(interaction: &Interaction) -> Option<InteractionSource> {
    match interaction {
        Interaction::Command(command) => Some(located_source(
            SourceKind::SlashCommand {
                name: command.data.name.clone(),
            },
            command.user.id.get(),
            command.channel_id.get(),
            command.guild_id.map(GuildId::get),
            None,
            command.guild_id.is_none(),
        )),
        Interaction::Modal(modal) => {
            let values = modal
                .data
                .components
                .iter()
                .filter_map(|row| {
                    row.components.iter().find_map(|component| match component {
                        ActionRowComponent::InputText(input) => Some(TextInput {
                            id: input.custom_id.clone(),
                            value: input.value.clone().unwrap_or_default(),
                        }),
                        _ => None,
                    })
                })
                .collect();
            Some(located_source(
                SourceKind::ModalSubmit {
                    custom_id: modal.data.custom_id.clone(),
                    values,
                },
                modal.user.id.get(),
                modal.channel_id.get(),
                modal.guild_id.map(GuildId::get),
                None,
                modal.guild_id.is_none(),
            ))
        }
        Interaction::Component(component) => Some(located_source(
            SourceKind::Component {
                custom_id: component.data.custom_id.clone(),
            },
            component.user.id.get(),
            component.channel_id.get(),
            component.guild_id.map(GuildId::get),
            Some(component.message.id.get()),
            component.guild_id.is_none(),
        )),
        _ => None,
    }
}

fn located_source(
    kind: SourceKind,
    user_id: u64,
    channel_id: u64,
    guild_id: Option<u64>,
    message_id: Option<u64>,
    is_dm: bool,
) -> InteractionSource {
    InteractionSource {
        kind,
        // Namespaced at the adapter boundary (SPEC-004 D-03).
        user_id: format!("discord:{user_id}"),
        destination: Destination::discord(channel_id),
        guild_id,
        message_id,
        is_dm,
    }
}

#[serenity::async_trait]
impl EventHandler for BotEvents {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "connected to discord");
        for (name, description) in [
            ("run", "Start a prompt run in a directory"),
            ("prompt", "Send a free-form prompt to a directory's session"),
            ("status", "Bot and server status"),
            ("history", "Action history for the current directory"),
            ("reset", "Reset the directory to a live commit"),
            ("release", "Release a held directory lock"),
            (
                "abort",
                "Interrupt the running action in the current directory",
            ),
            ("mute", "Toggle inbox reports for the current directory"),
            ("ls", "Browse directories under the configured roots"),
            ("cd", "Open a directory in the browser"),
        ] {
            let command = CreateCommand::new(name).description(description);
            if let Err(error) = ctx.http.create_global_command(&command).await {
                tracing::warn!(command = name, error = %error, "command registration failed");
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Some(source) = build_source(&interaction) else {
            return;
        };
        match router::guarded_plan(&self.ctx, &source).await {
            PlanOutcome::Deny => {
                respond_now(&self.http, &interaction, Some("not authorized")).await;
            }
            PlanOutcome::Ignore => respond_now(&self.http, &interaction, None).await,
            PlanOutcome::Failed(error) => {
                respond_now(&self.http, &interaction, Some(&format!("error: {error}"))).await;
            }
            PlanOutcome::Plan(plan) => self.act(&ctx, interaction, source, plan).await,
        }
    }

    async fn message(&self, _ctx: Context, message: Message) {
        // The one plain-message path in the bot (P14): replying to a posted
        // inbox report continues that session as an adopted action. Attachments
        // on any in-scope operator message are persisted (FR-36) and their
        // paths become the draft. Every other message is ignored — the operator
        // surface is interactions.
        let Some(operator) = self.ctx.guard.discord_operator() else {
            return;
        };
        if message.author.id.get() != operator || message.author.bot {
            return;
        }
        let channel = message.channel_id.get();
        let in_scope = message.guild_id.is_none()
            || self.ctx.guard.ops_channel.is_some_and(|ops| ops == channel)
            || self
                .ctx
                .guard
                .inbox_channel
                .is_some_and(|inbox| inbox == channel);
        if !in_scope {
            return;
        }
        for attachment in &message.attachments {
            match attachment.download().await {
                Ok(bytes) => {
                    if let Err(error) = handlers::files::ingest(
                        &self.ctx,
                        &format!("discord:{}", message.author.id.get()),
                        &attachment.filename,
                        &bytes,
                    )
                    .await
                    {
                        tracing::warn!(error = %error, "attachment ingest failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "attachment download failed");
                }
            }
        }
        let Some(reference) = message.referenced_message.clone() else {
            return;
        };
        let Ok(Some((session, _))) = self
            .ctx
            .store
            .lookup_msg_map("discord", &reference.id.get().to_string())
            .await
        else {
            return;
        };
        let text = message.content.clone();
        if text.trim().is_empty() {
            return;
        }
        if let Err(error) = handlers::adopt::adopt(
            &self.ctx,
            &session,
            &text,
            &Destination::discord(channel),
            &format!("discord:{}", message.author.id.get()),
        )
        .await
        {
            tracing::warn!(error = %error, "adoption failed");
        }
    }
}

/// Start the gateway with the given token/intents; returns the shard manager
/// so the drain path can shut it down cleanly.
pub async fn start_gateway(
    token: &str,
    intents: GatewayIntents,
    ctx: Arc<AppContext>,
) -> Result<Arc<ShardManager>, Box<serenity::Error>> {
    let http = shared_http(token, ctx.guard.discord_operator().unwrap_or(0));
    let mut client = Client::builder(token, intents)
        .event_handler(BotEvents::new(ctx, http))
        .await?;
    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        if let Err(error) = client.start().await {
            tracing::error!(error = %error, "gateway exited with error");
        }
    });
    Ok(shard_manager)
}
