import urllib.request
import json
from datetime import datetime

tickers = {'Tencent': '0700.HK', 'Meituan': '3690.HK', 'Alibaba': '9988.HK', 'Xiaomi': '1810.HK', 'NetEase': '9999.HK'}

for name, ticker in tickers.items():
    url = f"https://query1.finance.yahoo.com/v8/finance/chart/{ticker}?interval=1d&range=5d"
    req = urllib.request.Request(url, headers={'User-Agent': 'Mozilla/5.0'})
    try:
        with urllib.request.urlopen(req) as response:
            data = json.loads(response.read().decode())
            timestamps = data['chart']['result'][0]['timestamp']
            closes = data['chart']['result'][0]['indicators']['quote'][0]['close']
            
            print(f"--- {name} ({ticker}) ---")
            for t, c in zip(timestamps, closes):
                dt = datetime.fromtimestamp(t).strftime('%Y-%m-%d')
                print(f"{dt}: {c}")
    except Exception as e:
        print(f"Error fetching {name}: {e}")
