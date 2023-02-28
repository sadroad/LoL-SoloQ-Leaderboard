use dotenv::dotenv;
use futures::stream::StreamExt;
use redis::{AsyncCommands, Client};
use riven::consts::{PlatformRoute, QueueType};
use riven::models::league_v4::LeagueEntry;
use riven::RiotApi;
use std::cmp::Ordering;
use std::{
    env,
    error::Error,
    sync::{Arc, Mutex},
};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{ConfigBuilder, Event, EventTypeFlags, Shard, ShardId};
use twilight_http::Client as HttpClient;
use twilight_model::{
    application::{
        command::CommandType,
        interaction::{
            application_command::{CommandData, CommandOptionValue},
            InteractionData, InteractionType,
        },
    },
    channel::message::MessageFlags,
    gateway::{
        payload::outgoing::update_presence::UpdatePresencePayload,
        presence::{ActivityType, MinimalActivity, Status},
        Intents,
    },
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseType},
    id::{
        marker::{ApplicationMarker, ChannelMarker, MessageMarker},
        Id,
    },
};
use twilight_util::builder::embed::EmbedBuilder;
use twilight_util::builder::{
    command::{CommandBuilder, StringBuilder},
    InteractionResponseDataBuilder,
};

struct Context {
    http: Arc<HttpClient>,
    application_id: Id<ApplicationMarker>,
    riot_api: RiotApi,
    db: Client,
    scoreboard_channel: Arc<Mutex<Option<Id<ChannelMarker>>>>,
    scoreboard_message: Arc<Mutex<Option<Id<MessageMarker>>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    dotenv().ok();
    let token = env::var("DISCORD_TOKEN")?;
    let api_key = env::var("RIOT_API_KEY")?;
    let db_password = env::var("REDIS_PASSWORD")?;
    let db_hostname = env::var("REDIS_HOSTNAME")?;
    let db_port = env::var("REDIS_PORT")?;
    let db_url = format!(
        "redis://default:{}@{}:{}",
        db_password, db_hostname, db_port
    );

    let riot_api = RiotApi::new(api_key);

    let client = Client::open(db_url)?;
    let scoreboard_channel: Option<Id<ChannelMarker>> = {
        let mut conn = client.get_async_connection().await?;
        conn.get::<&str, Option<u64>>("scoreboard_channel")
            .await?
            .map(Id::new)
    };
    let scoreboard_message: Option<Id<MessageMarker>> = {
        let mut conn = client.get_async_connection().await?;
        conn.get::<&str, Option<u64>>("scoreboard_message")
            .await?
            .map(Id::new)
    };
    let config = ConfigBuilder::new(token.clone(), Intents::GUILD_MESSAGES)
        .presence(UpdatePresencePayload::new(
            vec![MinimalActivity {
                kind: ActivityType::Watching,
                name: "league players smh".into(),
                url: None,
            }
            .into()],
            false,
            None,
            Status::Online,
        )?)
        .event_types(
            EventTypeFlags::READY
                | EventTypeFlags::INTERACTION_CREATE
                | EventTypeFlags::MESSAGE_CREATE,
        )
        .build();

    let mut shard = Shard::with_config(ShardId::ONE, config);

    let http = Arc::new(HttpClient::new(token));

    let current_app = http.current_user_application().await?.model().await?;

    let interaction = http.interaction(current_app.id);

    // interaction.set_global_commands(&[]).await?;

    let commands = [
        CommandBuilder::new(
            "register",
            "Register a league account to track",
            CommandType::ChatInput,
        )
        .dm_permission(false)
        .option(
            StringBuilder::new(
                "username",
                "The summoner name to track. Only works for NA atm",
            )
            .required(true)
            .min_length(3)
            .max_length(16),
        )
        .build(),
        CommandBuilder::new(
            "leaderboard",
            "Make the current channel the scoreboard channel",
            CommandType::ChatInput,
        )
        .dm_permission(false)
        .default_member_permissions(Permissions::ADMINISTRATOR)
        .build(),
    ];

    interaction.set_global_commands(&commands).await?;

    let ctx = Arc::new(Context {
        http: Arc::clone(&http),
        application_id: current_app.id,
        riot_api,
        db: client,
        scoreboard_channel: Arc::new(Mutex::new(scoreboard_channel)),
        scoreboard_message: Arc::new(Mutex::new(scoreboard_message)),
    });
    let thread_ctx = Arc::clone(&ctx);
    tokio::spawn(async move {
        loop {
            update_scoreboard(&thread_ctx).await.unwrap();
            tokio::time::sleep(tokio::time::Duration::from_secs(60 * 60 * 24)).await;
        }
    });

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE)
        .build();

    loop {
        let event = match shard.next_event().await {
            Ok(event) => event,
            Err(source) => {
                tracing::warn!(?source, "error receiving event");

                if source.is_fatal() {
                    break;
                }

                continue;
            }
        };

        cache.update(&event);

        tokio::spawn(handle_event(event, Arc::clone(&ctx)));
    }

    Ok(())
}

async fn handle_event(event: Event, ctx: Arc<Context>) -> Result<(), Box<dyn Error + Send + Sync>> {
    match event {
        Event::InteractionCreate(interaction) => match interaction.kind {
            InteractionType::ApplicationCommand => {
                let data =
                    if let Some(InteractionData::ApplicationCommand(data)) = &interaction.data {
                        data
                    } else {
                        return Err("No application data".into());
                    };
                let name = data.name.as_str();
                tracing::info!("Slash command used: {}", name);
                match name {
                    "register" => {
                        let response = handle_register(data, &ctx).await;
                        ctx.http
                            .interaction(ctx.application_id)
                            .create_response(interaction.id, &interaction.token, &response)
                            .await?;
                    }
                    "leaderboard" => {
                        let response = InteractionResponse {
                            kind: InteractionResponseType::DeferredChannelMessageWithSource,
                            data: Some(
                                InteractionResponseDataBuilder::new()
                                    .flags(MessageFlags::EPHEMERAL)
                                    .build(),
                            ),
                        };
                        ctx.http
                            .interaction(ctx.application_id)
                            .create_response(interaction.id, &interaction.token, &response)
                            .await?;
                        let channel = interaction.channel_id.unwrap();
                        let mut conn = ctx.db.get_async_connection().await?;
                        conn.set::<&str, u64, String>("scoreboard_channel", channel.get())
                            .await
                            .unwrap();
                        {
                            let mut scoreboard = ctx.scoreboard_channel.lock().unwrap();
                            *scoreboard = Some(channel);
                        }
                        launch_scoreboard(&ctx).await?;
                        ctx.http
                            .interaction(ctx.application_id)
                            .update_response(&interaction.token)
                            .content(Some("Launched the scoreboard"))
                            .unwrap()
                            .await?;
                    }
                    _ => {
                        tracing::warn!("Unknown command: {}", name);
                        return Ok(());
                    }
                };
            }
            _ => {
                unimplemented!()
            }
        },
        Event::Ready(_) => tracing::info!("Bot is ready"),
        Event::MessageCreate(msg) => {
            if msg.channel_id == ctx.scoreboard_channel.lock().unwrap().unwrap() && !msg.author.bot
            {
                ctx.http.delete_message(msg.channel_id, msg.id).await?;
            }
        }
        _ => {
            unimplemented!()
        }
    }

    Ok(())
}

async fn launch_scoreboard(ctx: &Arc<Context>) -> Result<(), Box<dyn Error + Send + Sync>> {
    let channel = ctx.scoreboard_channel.lock().unwrap().unwrap();
    let message = ctx
        .http
        .create_message(channel)
        .content("Loading...")?
        .await?
        .model()
        .await?;
    {
        let mut scoreboard = ctx.scoreboard_message.lock().unwrap();
        *scoreboard = Some(message.id);
    }
    let mut conn = ctx.db.get_async_connection().await?;
    conn.set::<&str, u64, String>("scoreboard_message", message.id.get())
        .await
        .unwrap();
    update_scoreboard(ctx).await?;
    Ok(())
}

async fn update_scoreboard(ctx: &Arc<Context>) -> Result<(), Box<dyn Error + Send + Sync>> {
    {
        if ctx.scoreboard_channel.lock().unwrap().is_none()
            || ctx.scoreboard_message.lock().unwrap().is_none()
        {
            return Ok(());
        }
    }
    let mut conn = ctx.db.get_async_connection().await?;
    let iter = conn.scan::<String>().await?;
    let keys: Vec<String> = iter.collect().await;
    let mut accounts = vec![];
    //TODO probably need to make this caching of some sort to not get rate limited
    for key in keys {
        if !(key == "scoreboard_channel" || key == "scoreboard_message") {
            let summoner = ctx
                .riot_api
                .league_v4()
                .get_league_entries_for_summoner(PlatformRoute::NA1, &key)
                .await?;
            summoner.iter().for_each(|entry| {
                if entry.queue_type == QueueType::RANKED_SOLO_5x5 {
                    if entry.veteran {
                        tracing::info!("{} is a veteran", entry.summoner_name);
                    }
                    if entry.tier.unwrap().is_ranked() {
                        accounts.push(entry.clone());
                    }
                }
            });
        }
    }
    accounts.sort_by(compare);
    {
        let scoreboard_channel = *ctx.scoreboard_channel.lock().unwrap();
        let scoreboard_message = *ctx.scoreboard_message.lock().unwrap();
        let mut rank_content = String::new();
        rank_content.push_str(&format!(
            "`{:<2}` `{:^16}` `{:^14}` `{:^6}` `{:^4}` `{:^4}` `{:^3}`\n",
            "#", "Summoner", "Rank", "LP", "Win", "Loss", "WL%"
        ));
        for (idx, account) in accounts.iter().enumerate().take(10) {
            rank_content.push_str(&format!(
                "`{:<2}` `{:<16}` `{:<10} {:>3}` `{:>4}LP` `{:>3}W` `{:>3}L` `{}%`",
                idx + 1,
                account.summoner_name,
                account.tier.unwrap(),
                account.rank.unwrap(),
                account.league_points,
                account.wins,
                account.losses,
                ((account.wins as f64 / (account.wins + account.losses) as f64) * 100_f64) as i64
            ));
            if account.veteran {
                rank_content.push_str(" 👴\n");
            } else if account.hot_streak {
                rank_content.push_str(" 🔥\n");
            } else {
                rank_content.push('\n');
            }
        }
        dbg!(rank_content.len());
        let embed = EmbedBuilder::new()
            .title("OME SoloQ Leaderboard")
            .description(rank_content)
            .validate()?
            .build();
        ctx.http
            .update_message(scoreboard_channel.unwrap(), scoreboard_message.unwrap())
            .content(None)
            .unwrap()
            .embeds(Some(&[embed]))
            .unwrap()
            .await?;
    }
    Ok(())
}

fn compare(a: &LeagueEntry, b: &LeagueEntry) -> Ordering {
    let a_tier = a.tier.unwrap();
    let b_tier = b.tier.unwrap();
    match b_tier.cmp(&a_tier) {
        Ordering::Equal => {
            let a_division = a.rank.unwrap();
            let b_division = b.rank.unwrap();
            match b_division.cmp(&a_division) {
                Ordering::Equal => {
                    let a_lp = a.league_points;
                    let b_lp = b.league_points;
                    b_lp.cmp(&a_lp)
                }
                other => other,
            }
        }
        other => other,
    }
}

async fn handle_register(data: &CommandData, ctx: &Arc<Context>) -> InteractionResponse {
    let username = if let Some(opt) = data.options.get(0) {
        if let CommandOptionValue::String(username) = &opt.value {
            username.as_str()
        } else {
            return InteractionResponse {
                kind: InteractionResponseType::ChannelMessageWithSource,
                data: Some(
                    InteractionResponseDataBuilder::new()
                        .content("Invalid username")
                        .build(),
                ),
            };
        }
    } else {
        return InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(
                InteractionResponseDataBuilder::new()
                    .content("Invalid username")
                    .build(),
            ),
        };
    };
    let summoner = ctx
        .riot_api
        .summoner_v4()
        .get_by_summoner_name(PlatformRoute::NA1, username)
        .await
        .expect("Failed to lookup summoner");
    let summoner = match summoner {
        Some(summoner) => summoner,
        None => {
            return InteractionResponse {
                kind: InteractionResponseType::ChannelMessageWithSource,
                data: Some(
                    InteractionResponseDataBuilder::new()
                        .content("Invalid summoner name")
                        .build(),
                ),
            };
        }
    };
    let mut con = ctx.db.get_async_connection().await.unwrap();
    con.set_nx::<String, i32, i8>(summoner.id, 1).await.unwrap();
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(
            InteractionResponseDataBuilder::new()
                .flags(MessageFlags::EPHEMERAL)
                .content("Account Registered. It will be included on next refresh.")
                .build(),
        ),
    }
}
