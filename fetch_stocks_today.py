import urllib.request
import json
from datetime import datetime

tickers = {'Tencent': '0700.HK', 'Meituan': '3690.HK', 'Alibaba': '9988.HK', 'Xiaomi': '1810.HK', 'NetEase': '9999.HK'}

print(f"Current Time: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")

for name, ticker in tickers.items():
    url = f"https://query1.finance.yahoo.com/v8/finance/chart/{ticker}?interval=1m&range=1d"
    req = urllib.request.Request(url, headers={'User-Agent': 'Mozilla/5.0'})
    try:
        with urllib.request.urlopen(req) as response:
            data = json.loads(response.read().decode())
            meta = data['chart']['result'][0]['meta']
            price = meta['regularMarketPrice']
            prev_close = meta['previousClose']
            change = price - prev_close
            pct_change = (change / prev_close) * 100
            
            print(f"{name} ({ticker}): {price} ({pct_change:+.2f}%)")
    except Exception as e:
        print(f"Error fetching {name}: {e}")
