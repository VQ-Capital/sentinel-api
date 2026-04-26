# 🚪 sentinel-stream-gateway (Legacy: sentinel-api)

**Domain:** High-Throughput Binary Multiplexer & API Gateway
**Rol:** Sistemin Dışa Açılan Kapısı

İçerideki NATS JetStream omurgasını doğrudan internete açmak büyük bir güvenlik riskidir. Bu servis, NATS'taki (Trade, Report, Z-Score) verilerini dinler ve tek bir `StreamBundle` (Protobuf) zarfına koyarak WebSocket üzerinden Terminal'e (UI) aktarır.

- **Bağlantı Türü:** Asenkron Full-Duplex WebSocket
- **NATS Girdisi:** `*.>` (Tüm kanalları filtreleyerek okur)
- **NATS Çıktısı:** `control.command` (Sadece yetkili terminal komutları)