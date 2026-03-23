use anyhow::Result;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, FixedOffset, TimeZone, Utc, Weekday};
use serenity::{
    async_trait,
    builder::{CreateThread, GetMessages},
    model::{
        channel::{ChannelType, Message},
        gateway::Ready,
        id::{ChannelId, GuildId, MessageId},
    },
    prelude::*,
};
use std::collections::HashMap;
use std::env;
use tokio::time::{sleep, Duration};

struct Handler {
    target_guild: GuildId,
    target_channel: ChannelId,
}

/// 한국 시간대(KST, UTC+9) 오프셋
fn kst_offset() -> FixedOffset {
    FixedOffset::east_opt(9 * 60 * 60).expect("UTC+9 오프셋 생성 실패")
}

/// 현재 시각(UTC) 기준, "다음 월요일 09:00 (KST)" 시각을 UTC로 계산
fn next_monday_9_kst_as_utc(now_utc: DateTime<Utc>) -> DateTime<Utc> {
    let kst = kst_offset();
    let now_kst = now_utc.with_timezone(&kst);
    let weekday = now_kst.weekday();

    // 오늘 날짜의 09:00 (KST)
    let today_nine_kst = kst
        .with_ymd_and_hms(
            now_kst.year(),
            now_kst.month(),
            now_kst.day(),
            9,
            0,
            0,
        )
        .single()
        .expect("유효한 시간이어야 합니다.");

    // 오늘이 월요일이고, 아직 09:00(KST) 전이면 오늘 09:00(KST)에 실행
    if weekday == Weekday::Mon && now_kst < today_nine_kst {
        return today_nine_kst.with_timezone(&Utc);
    }

    // 그 외에는 "다음" 월요일 09:00(KST)을 찾는다.
    let days_from_monday = weekday.num_days_from_monday() as i64;

    // 오늘이 월요일(0)이면 7일 뒤, 그 외에는 (7 - days_from_monday)일 뒤가 다음 월요일
    let days_until_next_monday = if days_from_monday == 0 {
        7
    } else {
        7 - days_from_monday
    };

    let next_monday_kst = today_nine_kst + ChronoDuration::days(days_until_next_monday);
    next_monday_kst.with_timezone(&Utc)
}

 #[async_trait]
 impl EventHandler for Handler {
     // 필요 시 메시지 이벤트 처리도 추가 가능
     async fn message(&self, _ctx: Context, _msg: Message) {
         // 현재는 사용하지 않지만, 나중에 명령어 등을 붙이고 싶을 때 활용
     }

     async fn ready(&self, ctx: Context, ready: Ready) {
         println!("Logged in as {}", ready.user.name);

         // 봇이 준비되면 주간 체크 태스크를 시작한다.
         let guild_id = self.target_guild;
         let channel_id = self.target_channel;

         tokio::spawn(run_weekly_task(ctx.clone(), guild_id, channel_id));
     }
 }

/// 매주 월요일 09:00(UTC)에 스레드를 생성하고,
/// 해당 스레드의 메시지를 기준으로 출석/지각을 체크하는 태스크
async fn run_weekly_task(ctx: Context, guild_id: GuildId, channel_id: ChannelId) {
    // 직전 주 스레드 ID를 메모리에 유지한다.
    // (봇이 재시작되면 초기화되지만, 기본 동작에는 큰 문제 없음)
    let mut last_week_thread: Option<ChannelId> = None;

    loop {
        // 현재 시각 기준, "다음 월요일 09:00(KST)"까지 기다렸다가 실행
        let now_utc = Utc::now();
        let next_run = next_monday_9_kst_as_utc(now_utc);
        let diff = next_run - now_utc;
        let delay_secs = diff.num_seconds().max(0) as u64;

        sleep(Duration::from_secs(delay_secs)).await;

        let http = &ctx.http;

        // 1) 먼저 직전 주 스레드에 대해 결석/지각 체크를 진행한다.
        if let Some(prev_thread_id) = last_week_thread {
            if let Err(e) = check_inactive_users(&ctx, guild_id, prev_thread_id).await {
                eprintln!("weekly task error (check_inactive_users): {:?}", e);
            }
        }

        // 2) 그 다음, 이번 주에 사용할 새 스레드를 생성한다.
        let kst = kst_offset();
        let today_kst = Utc::now().with_timezone(&kst).date_naive();
        let next_week = today_kst + ChronoDuration::days(6);
        // 스레드 제목: "블로그\nMM/DD - MM/DD"
        let thread_name = format!(
            "블로그\n{} - {}",
            today_kst.format("%m/%d"),
            next_week.format("%m/%d")
        );

        match channel_id
            .create_thread(
                http,
                CreateThread::new(thread_name)
                    .kind(ChannelType::PublicThread),
            )
            .await
        {
            Ok(thread_channel) => {
                // 방금 만든 스레드를 다음 주 체크 대상으로 저장
                last_week_thread = Some(thread_channel.id);
            }
            Err(e) => {
                eprintln!("failed to create weekly thread: {:?}", e);
            }
        }
    }
}

/// 월요일 기준으로,
/// - 월요일 00:00까지 한 번도 작성하지 않은 유저: 결석(경고)
/// - 월요일 00:00~09:00 사이에 처음 작성한 유저: 지각
/// 를 태그해서 알려준다.
async fn check_inactive_users(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) -> Result<()> {
     let http = &ctx.http;

    // 1) 길드 멤버 전체 목록 가져오기 (페이지네이션)
     let mut after = None;
     let mut all_members = Vec::new();

     loop {
         let mut members = guild_id.members(http, Some(1000), after).await?;
         if members.is_empty() {
             break;
         }

         after = members.last().map(|m| m.user.id);
         all_members.append(&mut members);
     }

    // 2) "지난주 월요일 00:00 ~ 이번주 월요일 09:00" 구간의 메시지들에서
    //    각 유저의 "첫 메시지 시각"을 수집
    let now_utc = Utc::now();
    let kst = kst_offset();
    let now_kst = now_utc.with_timezone(&kst);

    // 기준: "이번주 월요일 00:00(KST)"
    let this_monday_midnight_kst = kst
        .with_ymd_and_hms(
            now_kst.year(),
            now_kst.month(),
            now_kst.day(),
            0,
            0,
            0,
        )
        .single()
        .expect("유효한 시간이어야 합니다.");

    // 비교는 모두 UTC로 하므로, 기준 시각을 다시 UTC로 변환
    let this_monday_midnight_utc = this_monday_midnight_kst.with_timezone(&Utc);
    let this_monday_9_kst = this_monday_midnight_kst + ChronoDuration::hours(9);
    let this_monday_9_utc = this_monday_9_kst.with_timezone(&Utc);
    // "지난주 월요일 09:00(KST)" 기준
    let last_monday_9_kst = this_monday_9_kst - ChronoDuration::days(7);
    let last_monday_9_utc = last_monday_9_kst.with_timezone(&Utc);

    // user_id -> 해당 주간 내 첫 메시지 시각 (UTC)
    let mut first_message_times: HashMap<serenity::model::id::UserId, DateTime<Utc>> = HashMap::new();
    let mut before: Option<MessageId> = None;

    'outer: loop {
        let mut builder = GetMessages::new().limit(100);

        if let Some(b) = before {
            builder = builder.before(b);
        }

        let messages = channel_id.messages(http, builder).await?;

        if messages.is_empty() {
            break;
        }

        for msg in &messages {
            if msg.timestamp < last_monday_9_utc.into() {
                // 지난주 월요일 09:00(KST 기준) 이전 메시지에 도달하면 중단
                break 'outer;
            }

            // 이번주 월요일 09:00(KST) 이후 메시지는 이번 체크 기준 밖이므로 무시
            if msg.timestamp > this_monday_9_utc.into() {
                continue;
            }

            if !msg.author.bot {
                let msg_time = msg.timestamp.to_utc();
                let entry = first_message_times
                    .entry(msg.author.id)
                    .or_insert(msg_time);

                if msg_time < *entry {
                    *entry = msg_time;
                }
            }
        }

        before = messages.last().map(|m| m.id);

    }

    // 3) 분류
    // - 지난주 월요일 09:00 ~ 이번주 월요일 09:00까지 한 번도 작성하지 않으면: 경고
    // - 지난주 월요일 09:00 ~ 이번주 월요일 00:00까지 작성하지 않았으나,
    //   이번주 월요일 00:00 ~ 09:00 사이에 처음 작성하면: 지각
    let mut warn = Vec::new(); // 완전 미참여 (경고)
    let mut late = Vec::new(); // 이번주 월요일 00:00~09:00 첫 작성 (지각)

    for member in all_members.into_iter().filter(|m| !m.user.bot) {
        if let Some(first_ts) = first_message_times.get(&member.user.id) {
            // 지난주 월요일 09:00 ~ 이번주 월요일 00:00 전에 작성한 경우 → 정상 참여로 간주
            if *first_ts < this_monday_midnight_utc {
                continue;
            }

            // 이번주 월요일 00:00 ~ 09:00 사이에 처음 작성 → 지각
            if *first_ts >= this_monday_midnight_utc && *first_ts <= this_monday_9_utc {
                late.push(member);
            } else {
                // 그 외 (예: 이번주 월요일 09:00 이후에 처음 작성) → 이번 체크 기준에서는
                // "이번주 월요일 09시까지 작성하지 않음"이므로 경고
                warn.push(member);
            }
        } else {
            // 지난주 월요일 09:00 ~ 이번주 월요일 09:00까지 메시지가 전혀 없음 → 경고
            warn.push(member);
        }
    }

    if warn.is_empty() && late.is_empty() {
        channel_id
            .say(http, "지난주 월요일 09:00부터 이번주 월요일 09:00까지 모두 참여해서, 경고나 지각 대상자가 없습니다!")
            .await?;
        return Ok(());
    }

    let mut parts = Vec::new();

    if !warn.is_empty() {
        let mention_list = warn
            .iter()
            .map(|m| format!("<@{}>", m.user.id))
            .collect::<Vec<_>>()
            .join(" ");

        parts.push(format!(
            "지난주 월요일 09:00부터 이번주 월요일 09:00까지 이 스레드에 메시지를 남기지 않아 **경고 1회**를 받은 사람들:\n{}",
            mention_list
        ));
    }

    if !late.is_empty() {
        let mention_list = late
            .iter()
            .map(|m| format!("<@{}>", m.user.id))
            .collect::<Vec<_>>()
            .join(" ");

        parts.push(format!(
            "이번주 월요일 00:00~09:00 사이에 처음으로 메시지를 남겨 **지각 1회**를 받은 사람들:\n{}",
            mention_list
        ));
    }

    let content = parts.join("\n\n");
    channel_id.say(http, content).await?;

    Ok(())
}

 #[tokio::main]
 async fn main() -> Result<()> {
     dotenvy::dotenv().ok();

     // 환경변수에서 토큰/ID 불러오기
     let token = env::var("DISCORD_TOKEN")
         .expect("DISCORD_TOKEN 환경변수를 설정해주세요.");
     let guild_id: u64 = env::var("TARGET_GUILD_ID")
         .expect("TARGET_GUILD_ID 환경변수를 설정해주세요.")
         .parse()
         .expect("TARGET_GUILD_ID 는 u64 숫자여야 합니다.");
     let channel_id: u64 = env::var("TARGET_CHANNEL_ID")
         .expect("TARGET_CHANNEL_ID 환경변수를 설정해주세요.")
         .parse()
         .expect("TARGET_CHANNEL_ID 는 u64 숫자여야 합니다.");

     let intents = GatewayIntents::GUILD_MEMBERS
         | GatewayIntents::GUILD_MESSAGES
         | GatewayIntents::MESSAGE_CONTENT;

    let handler = Handler {
        target_guild: GuildId::new(guild_id),
        target_channel: ChannelId::new(channel_id),
    };

     let mut client = Client::builder(&token, intents)
         .event_handler(handler)
         .await
         .expect("Client 생성 실패");

     if let Err(why) = client.start().await {
         eprintln!("Client error: {:?}", why);
     }

     Ok(())
 }

