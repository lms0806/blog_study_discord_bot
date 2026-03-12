use anyhow::Result;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Timelike, Utc, Weekday};
use serenity::{
    async_trait,
    builder::GetMessages,
    model::{
        channel::Message,
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

/// 현재 시각 기준, "다음 월요일 09:00 (UTC)" DateTime을 계산
fn next_monday_9_utc(now: DateTime<Utc>) -> DateTime<Utc> {
    let weekday = now.weekday();

    // 오늘 날짜의 09:00 (UTC)
    let today_nine = now
        .date_naive()
        .and_hms_opt(9, 0, 0)
        .expect("유효한 시간이어야 합니다.")
        .and_utc();

    // 오늘이 월요일이고, 아직 09:00 전이면 오늘 09:00에 실행
    if weekday == Weekday::Mon && now < today_nine {
        return today_nine;
    }

    // 그 외에는 "다음" 월요일 09:00을 찾는다.
    let days_from_monday = weekday.num_days_from_monday() as i64;

    // 오늘이 월요일(0)이면 7일 뒤, 그 외에는 (7 - days_from_monday)일 뒤가 다음 월요일
    let days_until_next_monday = if days_from_monday == 0 {
        7
    } else {
        7 - days_from_monday
    };

    today_nine + ChronoDuration::days(days_until_next_monday)
}

 #[async_trait]
 impl EventHandler for Handler {
     async fn ready(&self, ctx: Context, ready: Ready) {
         println!("Logged in as {}", ready.user.name);

         // 봇이 준비되면 주간 체크 태스크를 시작한다.
         let guild_id = self.target_guild;
         let channel_id = self.target_channel;

         tokio::spawn(run_weekly_task(ctx.clone(), guild_id, channel_id));
     }

     // 필요 시 메시지 이벤트 처리도 추가 가능
     async fn message(&self, _ctx: Context, _msg: Message) {
         // 현재는 사용하지 않지만, 나중에 명령어 등을 붙이고 싶을 때 활용
     }
 }

/// 매주 월요일 09:00(UTC)에 비활성 유저를 체크하는 태스크
 async fn run_weekly_task(ctx: Context, guild_id: GuildId, channel_id: ChannelId) {
    // 1) 현재 시각 기준, 다음 월요일 09:00(UTC)까지 기다린다.
    let now = Utc::now();
    let first_run = next_monday_9_utc(now);
    let diff = first_run - now;
    // 음수가 되는 경우를 방지해서 최소 0초로 맞춘다.
    let initial_delay_secs = diff.num_seconds().max(0) as u64;

    sleep(Duration::from_secs(initial_delay_secs)).await;

    // 2) 첫 실행 이후에는 매주(7일 간격) 한 번씩 반복
    // 7일(초 단위)
    let weekly_interval = Duration::from_secs(7 * 24 * 60 * 60);

    loop {
        if let Err(e) = check_inactive_users(&ctx, guild_id, channel_id).await {
            eprintln!("weekly task error: {:?}", e);
        }

        sleep(weekly_interval).await;
    }
 }

 /// 지난 1주일 동안 target_channel 에서 한 번도 발언하지 않은 길드 멤버들을 태그
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

    // 2) 지난 1주일 동안의 메시지들에서 "각 유저의 첫 메시지 시각" 수집
     let now = Utc::now();
     let one_week_ago = now - ChronoDuration::days(7);
    let today_midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("유효한 시간이어야 합니다.")
        .and_utc();

   // user_id -> 지난 1주일 내 첫 메시지 시각
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
            if msg.timestamp < one_week_ago.into() {
                 // 1주 이전 메시지에 도달하면 중단
                 break 'outer;
             }

             if !msg.author.bot {
                 let entry = first_message_times
                     .entry(msg.author.id)
                     .or_insert(msg.timestamp);

                 if msg.timestamp < *entry {
                     *entry = msg.timestamp;
                 }
             }
         }

        before = messages.last().map(|m| m.id);

        if let Some(last) = messages.last() {
            if last.timestamp < one_week_ago.into() {
                break;
            }
        }
     }

     // 3) 비활성(발언하지 않은) 유저, 지각(월요일 00:00~09:00에만 작성) 유저 분류
     let mut absent = Vec::new(); // 완전 미참여 (경고 1회)
     let mut late = Vec::new();   // 월요일 00:00~09:00 사이에 첫 메시지 (지각)

     for member in all_members.into_iter().filter(|m| !m.user.bot) {
         if let Some(first_ts) = first_message_times.get(&member.user.id) {
             // 지난 1주일 내에 메시지는 있지만, 그 첫 메시지가 오늘(월요일) 00:00~09:00 사이라면 지각
             if *first_ts >= today_midnight && *first_ts <= now {
                 late.push(member);
             }
         } else {
             // 지난 1주일 동안 메시지가 전혀 없음 → 결석
             absent.push(member);
         }
     }

     if absent.is_empty() && late.is_empty() {
         channel_id
             .say(http, "지난 1주일 동안 이 채널에는 모두가 참여했습니다!")
             .await?;
         return Ok(());
     }

     let mut parts = Vec::new();

     if !absent.is_empty() {
         let mention_list = absent
             .iter()
             .map(|m| format!("<@{}>", m.user.id))
             .collect::<Vec<_>>()
             .join(" ");

         parts.push(format!(
             "지난 1주일 동안 (월요일 09:00 기준) 이 채널에 메시지를 남기지 않아 **경고 1회 (결석)**를 받은 사람들:\n{}",
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
             "지난 1주일 동안 월요일 00:00~09:00 사이에 처음으로 메시지를 남겨 **지각 1회**를 받은 사람들:\n{}",
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

