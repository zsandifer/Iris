import './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { photos, dailyPostsOn, dailyPostsSubtitle } from '../../web/js/feed.js';

test('pipeline photos use checked local copies in the feed and Saved', () => {
  const originals = ['https://example.com/original.jpg'];
  assert.deepEqual(photos({ images: originals, photos: ['/api/feed/images/photo.jpg'] }), ['/api/feed/images/photo.jpg']);
  assert.deepEqual(photos({ images: originals, image_url: originals[0], photos: [] }), [], 'an empty checked-photo list never falls back to a third-party request');
  assert.deepEqual(photos({ images: originals }), originals, 'older agent posts still show their source photos');
});

test('Daily posts reflects the effective enable choice and pause', () => {
  assert.equal(dailyPostsOn({ enabled: false, paused: false }), false);
  assert.equal(dailyPostsSubtitle({ enabled: false, paused: false }), 'Off');
  assert.equal(dailyPostsOn({ enabled: true, paused: true }), false);
  assert.equal(dailyPostsSubtitle({ enabled: true, paused: true }), 'Paused');
  assert.equal(dailyPostsOn({ enabled: true, paused: false }), true);
  assert.equal(dailyPostsSubtitle({ enabled: true, paused: false }), 'First batch coming up');
  assert.equal(dailyPostsOn(undefined), false);
});
